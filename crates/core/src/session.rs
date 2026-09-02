//! The on-disk session: metadata plus an append-only event log.
//!
//! Durability model: `events.jsonl` is opened once and appended to, one JSON
//! object per line. A line is complete or it is not — a crash mid-recording
//! costs at most the final line, and readers skip an unparseable trailing line
//! rather than failing the whole session. That property is why the event stream
//! is JSONL and not a single JSON document.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::clock::{epoch_ms, to_at_ms, EpochMs};
use crate::events::{EventInput, RecEvent};
use crate::paths;

/// Where and when a recording was handed to a TeachOnce server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Submission {
    /// The server's base URL as configured in Settings.
    pub server: String,
    pub at: EpochMs,
}

/// `session.json` — everything needed to interpret the rest of the folder.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    pub id: String,
    /// Wall clock when Record was pressed. The origin of every `at_ms`.
    pub started_at: EpochMs,
    pub stopped_at: Option<EpochMs>,
    pub platform: String,
    pub app_version: String,
    /// Set once the user turned the microphone on at least once.
    #[serde(default)]
    pub narrated: bool,
    /// Human-facing name, filled in by analysis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Set when the recording has been submitted to a server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submitted: Option<Submission>,
}

impl SessionMeta {
    pub fn duration_ms(&self) -> i64 {
        self.stopped_at.unwrap_or_else(epoch_ms) - self.started_at
    }
}

/// Generate a sortable, collision-resistant session id: `YYYYMMDD-HHMMSS-<rand>`.
///
/// The timestamp prefix means a plain directory listing is in recording order,
/// which matters more than it sounds when you are debugging a user's data folder.
pub fn new_session_id() -> String {
    let now = time::OffsetDateTime::now_local()
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    let stamp = format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        now.year(),
        now.month() as u8,
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    );
    let suffix: String = uuid::Uuid::new_v4().simple().to_string().chars().take(8).collect();
    format!("{stamp}-{suffix}")
}

/// An open session being written to.
pub struct SessionStore {
    meta: SessionMeta,
    dir: PathBuf,
    log: File,
    seq: u64,
}

impl SessionStore {
    /// Create the session folder and open its event log.
    pub fn create(app_version: &str) -> Result<Self> {
        let meta = SessionMeta {
            id: new_session_id(),
            started_at: epoch_ms(),
            stopped_at: None,
            platform: std::env::consts::OS.to_string(),
            app_version: app_version.to_string(),
            narrated: false,
            title: None,
            submitted: None,
        };
        let dir = paths::sessions_root()?.join(&meta.id);
        std::fs::create_dir_all(dir.join("frames"))
            .with_context(|| format!("creating session folder {}", dir.display()))?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("events.jsonl"))?;
        let store = Self { meta, dir, log, seq: 0 };
        store.write_meta()?;
        Ok(store)
    }

    pub fn meta(&self) -> &SessionMeta {
        &self.meta
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn frames_dir(&self) -> PathBuf {
        self.dir.join("frames")
    }

    pub fn audio_dir(&self) -> PathBuf {
        self.dir.join("audio")
    }

    pub fn event_count(&self) -> u64 {
        self.seq
    }

    /// Stamp and append one event. Persistence failures are surfaced to the
    /// caller, which logs them — a collector must never abort a recording
    /// because one event could not be written.
    pub fn append(&mut self, input: EventInput) -> Result<RecEvent> {
        let epoch = epoch_ms();
        self.seq += 1;
        let event = RecEvent {
            seq: self.seq,
            t: to_at_ms(epoch, self.meta.started_at),
            epoch,
            source: input.source.to_string(),
            payload: input.payload,
        };
        let mut line = serde_json::to_string(&event)?;
        line.push('\n');
        self.log.write_all(line.as_bytes())?;
        Ok(event)
    }

    /// Note that the microphone was used, so the library can show a narration
    /// badge without opening the audio folder.
    pub fn mark_narrated(&mut self) -> Result<()> {
        if !self.meta.narrated {
            self.meta.narrated = true;
            self.write_meta()?;
        }
        Ok(())
    }

    /// Close the session: flush the log and stamp the stop time.
    pub fn finalize(mut self) -> Result<SessionMeta> {
        self.log.flush()?;
        self.meta.stopped_at = Some(epoch_ms());
        self.write_meta()?;
        Ok(self.meta)
    }

    fn write_meta(&self) -> Result<()> {
        write_json(&self.dir.join("session.json"), &self.meta)
    }
}

/// Write JSON atomically (temp file + rename), so a reader never observes a
/// half-written document.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    /// Distinguishes concurrent writers *within* one process. The pid alone is
    /// not enough: two threads writing the same path would otherwise share a
    /// temp file and race to rename a partially-written one into place.
    static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        WRITE_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, serde_json::to_string_pretty(value)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Read a JSON file, returning `None` when it is absent or unparseable.
pub fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let raw = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&raw) {
        Ok(value) => Some(value),
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "ignoring unparseable JSON");
            None
        }
    }
}

/// Parse `events.jsonl`, skipping any malformed line.
pub fn read_events(path: &Path) -> Vec<RecEvent> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<RecEvent>(line) {
            Ok(event) => out.push(event),
            // A partial trailing line is the expected shape of a crash, not a
            // reason to discard the recording.
            Err(err) => tracing::debug!(%err, "skipping malformed event line"),
        }
    }
    out
}

/// Metadata for one recording in the library list.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    #[serde(flatten)]
    pub meta: SessionMeta,
    pub event_count: usize,
    pub frame_count: usize,
    pub has_transcript: bool,
    pub has_analysis: bool,
    pub has_skill: bool,
}

/// List every recording, newest first.
pub fn list_sessions() -> Result<Vec<SessionSummary>> {
    let root = paths::sessions_root()?;
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&root)? {
        let dir = entry?.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(mut meta) = read_json::<SessionMeta>(&dir.join("session.json")) else {
            continue;
        };
        // A recording analysed before titles were copied into session.json
        // still has its name — in analysis.json. Read it from there, so the
        // list never says "Untitled" for something that was named.
        if meta.title.is_none() {
            meta.title = read_json::<crate::analysis::Analysis>(&dir.join("analysis.json"))
                .map(|analysis| analysis.title)
                .filter(|title| !title.trim().is_empty());
        }
        let frame_count = std::fs::read_dir(dir.join("frames"))
            .map(|d| d.filter_map(Result::ok).count())
            .unwrap_or(0);
        out.push(SessionSummary {
            event_count: read_events(&dir.join("events.jsonl")).len(),
            frame_count,
            has_transcript: dir.join("narration.json").exists(),
            has_analysis: dir.join("analysis.json").exists(),
            has_skill: dir.join("skill.json").exists(),
            meta,
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.meta.started_at));
    Ok(out)
}

/// Give a recording its human-facing title, or clear it with `None` or blank.
///
/// The library list reads `session.json` alone, so a title produced by analysis
/// (or typed by the user) has to be copied here before it shows up anywhere but
/// the detail view.
pub fn set_session_title(dir: &Path, title: Option<&str>) -> Result<()> {
    let path = dir.join("session.json");
    let mut meta: SessionMeta = read_json(&path)
        .with_context(|| format!("{} has no readable session.json", dir.display()))?;
    meta.title = title.map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
    write_json(&path, &meta)
}

/// Record that a recording was submitted to `server`.
pub fn mark_submitted(dir: &Path, server: &str) -> Result<()> {
    let path = dir.join("session.json");
    let mut meta: SessionMeta = read_json(&path)
        .with_context(|| format!("{} has no readable session.json", dir.display()))?;
    meta.submitted = Some(Submission { server: server.trim_end_matches('/').to_string(), at: epoch_ms() });
    write_json(&path, &meta)
}

/// The model-free reconstruction: always produce `bundle.json` and
/// `description.md` from the events on disk.
///
/// This runs the moment a recording stops, and again on a server the moment a
/// recording arrives, so a recording is never just an opaque folder of JSON.
pub fn reconstruct_session(dir: &Path) -> Result<()> {
    let meta: SessionMeta = read_json(&dir.join("session.json"))
        .context("this recording has no readable session.json")?;
    let events = read_events(&dir.join("events.jsonl"));
    let bundle = crate::timeline::build_bundle(&meta, &events);
    let narration: Option<crate::narration::NarrationTranscript> =
        read_json(&dir.join("narration.json"));

    write_json(&dir.join("bundle.json"), &bundle).context("writing bundle.json")?;
    std::fs::write(
        dir.join("description.md"),
        crate::describe::render_description(&bundle, narration.as_ref()),
    )
    .context("writing description.md")?;

    tracing::info!(
        steps = bundle.stats.step_count,
        events = bundle.stats.event_count,
        frames = bundle.stats.frame_count,
        "reconstruction complete"
    );
    Ok(())
}

/// Permanently delete a recording and everything in it.
pub fn delete_session(id: &str) -> Result<()> {
    let dir = paths::session_dir(id)?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("deleting {}", dir.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventPayload;

    /// `SKILLREC_DATA_DIR` is process-global, so tests that redirect it must not
    /// run concurrently — one test's sessions would show up in another's listing.
    static DATA_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct TempData {
        dir: PathBuf,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl TempData {
        fn new(name: &str) -> Self {
            let guard = DATA_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = std::env::temp_dir().join(format!("skillrec-{name}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            unsafe { std::env::set_var("SKILLREC_DATA_DIR", &dir) };
            Self { dir, _guard: guard }
        }
    }

    impl Drop for TempData {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
            unsafe { std::env::remove_var("SKILLREC_DATA_DIR") };
        }
    }

    #[test]
    fn session_ids_sort_chronologically_and_are_path_safe() {
        let id = new_session_id();
        assert!(paths::is_valid_session_id(&id), "{id} must be a safe path segment");
        assert_eq!(id.len(), 8 + 1 + 6 + 1 + 8);
    }

    #[test]
    fn appended_events_are_sequenced_and_readable_back() {
        let _data = TempData::new("store");
        let mut store = SessionStore::create("0.1.0-test").unwrap();
        let dir = store.dir().to_path_buf();

        store
            .append(EventInput::new(
                "test",
                EventPayload::AppActivate {
                    app: "Safari".into(),
                    title: "Pricing".into(),
                    url: None,
                    host: None,
                    bundle_id: None,
                    pid: None,
                    bounds: None,
                },
            ))
            .unwrap();
        store
            .append(EventInput::new("test", EventPayload::Marker { note: "here".into() }))
            .unwrap();
        assert_eq!(store.event_count(), 2);
        store.finalize().unwrap();

        let events = read_events(&dir.join("events.jsonl"));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seq, 1);
        assert_eq!(events[1].seq, 2);
        assert!(events[0].t <= events[1].t);
        assert_eq!(events[0].app(), Some("Safari"));
    }

    #[test]
    fn a_truncated_final_line_does_not_lose_the_recording() {
        let _data = TempData::new("truncated");
        let mut store = SessionStore::create("0.1.0-test").unwrap();
        let dir = store.dir().to_path_buf();
        store
            .append(EventInput::new("test", EventPayload::Marker { note: "kept".into() }))
            .unwrap();
        drop(store);

        // Simulate a crash partway through writing the next event.
        let log = dir.join("events.jsonl");
        let mut file = OpenOptions::new().append(true).open(&log).unwrap();
        file.write_all(b"{\"seq\":2,\"t\":10,\"epo").unwrap();

        let events = read_events(&log);
        assert_eq!(events.len(), 1, "the complete line survives the partial one");
    }

    #[test]
    fn finalize_stamps_a_stop_time_and_the_library_lists_it() {
        let _data = TempData::new("list");
        let store = SessionStore::create("0.1.0-test").unwrap();
        let id = store.meta().id.clone();
        let meta = store.finalize().unwrap();
        assert!(meta.stopped_at.is_some());

        let sessions = list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].meta.id, id);
        assert!(!sessions[0].has_analysis);

        delete_session(&id).unwrap();
        assert!(list_sessions().unwrap().is_empty());
    }

    #[test]
    fn a_title_set_after_the_fact_reaches_the_library_list() {
        // The describer names a recording only after it is closed, and the
        // list is built from session.json alone — so the title must land there.
        let _data = TempData::new("title");
        let store = SessionStore::create("0.1.0-test").unwrap();
        let dir = store.dir().to_path_buf();
        let id = store.meta().id.clone();
        store.finalize().unwrap();
        assert!(list_sessions().unwrap()[0].meta.title.is_none());

        set_session_title(&dir, Some("  Check Pricing  ")).unwrap();
        assert_eq!(list_sessions().unwrap()[0].meta.title.as_deref(), Some("Check Pricing"));

        // Blank is "no title", not an empty string the UI would render as nothing.
        set_session_title(&dir, Some("   ")).unwrap();
        assert!(list_sessions().unwrap()[0].meta.title.is_none());

        assert!(set_session_title(&dir.join("missing"), Some("x")).is_err());
        delete_session(&id).unwrap();
    }

    #[test]
    fn a_submission_is_recorded_and_listed() {
        let _data = TempData::new("submit");
        let store = SessionStore::create("0.1.0-test").unwrap();
        let dir = store.dir().to_path_buf();
        store.finalize().unwrap();
        assert!(list_sessions().unwrap()[0].meta.submitted.is_none());

        mark_submitted(&dir, "http://server.local:7777/").unwrap();
        let listed = list_sessions().unwrap();
        let submission = listed[0].meta.submitted.as_ref().unwrap();
        assert_eq!(submission.server, "http://server.local:7777");
        assert!(submission.at > 0);
    }

    #[test]
    fn a_recording_analysed_before_titles_were_stored_is_still_named_in_the_list() {
        // Seen live: sessions analysed by an earlier build had a title in
        // analysis.json and none in session.json, and listed as "Untitled".
        let _data = TempData::new("backfill");
        let store = SessionStore::create("0.1.0-test").unwrap();
        let dir = store.dir().to_path_buf();
        store.finalize().unwrap();

        let analysis =
            crate::analysis::Analysis { title: "Audit API Keys".into(), ..Default::default() };
        write_json(&dir.join("analysis.json"), &analysis).unwrap();
        assert_eq!(list_sessions().unwrap()[0].meta.title.as_deref(), Some("Audit API Keys"));

        // Once session.json has a title of its own, that one wins.
        set_session_title(&dir, Some("Renamed")).unwrap();
        assert_eq!(list_sessions().unwrap()[0].meta.title.as_deref(), Some("Renamed"));
    }
}
