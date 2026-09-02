//! The recorder state machine.
//!
//! Every lifecycle mutation — start, stop, discard, mic on, mic off, app
//! shutdown — goes through one lock, because they genuinely race: the tray, the
//! global hotkey, the UI button and the window-close handler can all fire at the
//! same moment, and two of them starting a recording at once would leave a
//! session folder nobody owns.
//!
//! The other rule is that **capture never fails a recording**. A missing screen
//! permission, an unavailable microphone, an unwritable frame — each is logged
//! and degrades that one signal. The event stream is the primary source and it
//! keeps going.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Serialize;
use skillrec_capture::audio::{AudioManifest, AudioSegment, MicrophoneRecorder};
use skillrec_capture::collector::{Collector, CollectorHost};
use skillrec_capture::{ActiveWindowCollector, ClipboardCollector, ScreenCollector};
use skillrec_core::config::CaptureConfig;
use skillrec_core::events::{EventInput, EventPayload};
use skillrec_core::session::{read_events, read_json, write_json, SessionMeta, SessionStore};
use tokio::sync::{mpsc, Mutex};

/// Whether a microphone is running, and why not if it is not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "state", content = "detail")]
pub enum MicrophoneState {
    Off,
    On { device: String },
    Error { message: String },
}

/// What the UI renders.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecorderStatus {
    pub recording: bool,
    pub session_id: Option<String>,
    pub started_at: Option<i64>,
    pub event_count: u64,
    pub microphone: MicrophoneState,
    /// The last saved recording, so the library can jump straight to it.
    pub last_session_id: Option<String>,
}

/// One live recording.
struct Active {
    store: Arc<Mutex<SessionStore>>,
    host: CollectorHost,
    drain: tokio::task::JoinHandle<()>,
    dir: PathBuf,
    id: String,
    started_at: i64,
    microphone: Option<MicrophoneRecorder>,
    microphone_state: MicrophoneState,
    audio_segments: Vec<AudioSegment>,
}

/// Owns the recording lifecycle.
pub struct Recorder {
    active: Mutex<Option<Active>>,
    app_version: String,
    last_session: Mutex<Option<String>>,
}

impl Recorder {
    pub fn new(app_version: impl Into<String>) -> Self {
        Self {
            active: Mutex::new(None),
            app_version: app_version.into(),
            last_session: Mutex::new(None),
        }
    }

    pub async fn status(&self) -> RecorderStatus {
        let active = self.active.lock().await;
        let last_session_id = self.last_session.lock().await.clone();
        match active.as_ref() {
            Some(session) => RecorderStatus {
                recording: true,
                session_id: Some(session.id.clone()),
                started_at: Some(session.started_at),
                event_count: session.store.lock().await.event_count(),
                microphone: session.microphone_state.clone(),
                last_session_id,
            },
            None => RecorderStatus {
                recording: false,
                session_id: None,
                started_at: None,
                event_count: 0,
                microphone: MicrophoneState::Off,
                last_session_id,
            },
        }
    }

    pub async fn is_recording(&self) -> bool {
        self.active.lock().await.is_some()
    }

    /// Begin a recording.
    ///
    /// `device` names the microphone to narrate with when `narrate` is set;
    /// `None` means the system default.
    pub async fn start(
        &self,
        config: CaptureConfig,
        narrate: bool,
        device: Option<String>,
    ) -> Result<String> {
        self.start_with(build_collectors(&config), narrate, device).await
    }

    /// [`start`](Self::start) with an explicit collector set, so the lifecycle
    /// can be exercised without a screen, a clipboard or a microphone.
    async fn start_with(
        &self,
        collectors: Vec<Box<dyn Collector>>,
        narrate: bool,
        device: Option<String>,
    ) -> Result<String> {
        let mut active = self.active.lock().await;
        anyhow::ensure!(active.is_none(), "a recording is already running");

        let mut store = SessionStore::create(&self.app_version)?;
        let id = store.meta().id.clone();
        let dir = store.dir().to_path_buf();
        let started_at = store.meta().started_at;

        store
            .append(EventInput::new(
                "recorder",
                EventPayload::SessionStart { platform: std::env::consts::OS.to_string() },
            ))
            .ok();

        let store = Arc::new(Mutex::new(store));
        let (tx, mut rx) = mpsc::unbounded_channel();

        // One task owns the store, so collectors on several threads never
        // contend for the log file and events keep their arrival order.
        let sink = Arc::clone(&store);
        let drain = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if let Err(err) = sink.lock().await.append(event) {
                    tracing::warn!("could not persist an event: {err:#}");
                }
            }
        });

        let host = CollectorHost::start(collectors, tx, dir.clone(), started_at);
        tracing::info!(%id, "recording started");

        let mut session = Active {
            store,
            host,
            drain,
            dir,
            id: id.clone(),
            started_at,
            microphone: None,
            microphone_state: MicrophoneState::Off,
            audio_segments: Vec::new(),
        };

        if narrate {
            // A microphone failure must not abort the recording — the user gets
            // an error badge and a session without narration.
            session.microphone_state = start_microphone(&mut session, device.as_deref());
            if matches!(session.microphone_state, MicrophoneState::On { .. }) {
                note_narration(&session, true).await;
            }
        }

        *active = Some(session);
        Ok(id)
    }

    /// Turn the microphone on or off mid-recording.
    pub async fn set_microphone(&self, on: bool, device: Option<String>) -> Result<MicrophoneState> {
        let mut active = self.active.lock().await;
        let session = active.as_mut().context("no recording is running")?;

        if on {
            if matches!(session.microphone_state, MicrophoneState::On { .. }) {
                return Ok(session.microphone_state.clone());
            }
            session.microphone_state = start_microphone(session, device.as_deref());
            if matches!(session.microphone_state, MicrophoneState::On { .. }) {
                note_narration(session, true).await;
            }
        } else {
            let was_on = session.microphone.is_some();
            stop_microphone(session).await;
            if was_on {
                note_narration(session, false).await;
            }
        }
        Ok(session.microphone_state.clone())
    }

    /// Stop and keep the recording. Returns the session id once the
    /// reconstruction (`bundle.json`, `description.md`) is on disk.
    pub async fn stop(&self) -> Result<String> {
        let session = self.finish().await?;
        let id = session.id.clone();
        let dir = session.dir.clone();

        // Post-processing is deterministic and takes milliseconds even for a
        // long recording. It is awaited rather than fired off because the UI
        // jumps to the recording the moment this returns, and would otherwise
        // find no description there half the time.
        match tokio::task::spawn_blocking(move || process_session(&dir)).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::warn!("post-processing failed: {err:#}"),
            Err(err) => tracing::warn!("post-processing did not run: {err}"),
        }

        *self.last_session.lock().await = Some(id.clone());
        Ok(id)
    }

    /// Stop and delete the recording.
    pub async fn discard(&self) -> Result<String> {
        let session = self.finish().await?;
        let id = session.id.clone();
        skillrec_core::session::delete_session(&id)?;
        tracing::info!(%id, "recording discarded");
        Ok(id)
    }

    /// Shared teardown: stop producers, drain, flush, finalize.
    async fn finish(&self) -> Result<FinishedSession> {
        let mut active = self.active.lock().await;
        let mut session = active.take().context("no recording is running")?;

        // Order matters. The microphone is flushed first so its stop boundary
        // is as close as possible to the click. Then the collectors are joined,
        // which writes the frame manifest and drops the last event senders.
        // Then the drain is awaited, so everything the collectors sent is on
        // disk. Only then is the stop marker appended — last, where it belongs.
        stop_microphone(&mut session).await;

        // Joining collector threads blocks — a browser mid-AppleScript can hold
        // one for most of a second — so it runs on the blocking pool rather
        // than stalling an async worker.
        let host = session.host;
        if let Err(err) = tokio::task::spawn_blocking(move || host.stop()).await {
            tracing::warn!("joining the collectors failed: {err}");
        }

        // With every sender gone the drain ends by itself once its queue is
        // empty. It is awaited, not aborted: aborting could discard whatever a
        // collector sent in its final moments, such as the last screen frame.
        if let Err(err) = session.drain.await {
            tracing::warn!("the event drain failed: {err}");
        }

        session
            .store
            .lock()
            .await
            .append(EventInput::new("recorder", EventPayload::SessionStop {}))
            .ok();

        if !session.audio_segments.is_empty() {
            write_json(
                &session.dir.join("audio.json"),
                &AudioManifest { segments: session.audio_segments.clone() },
            )
            .ok();
        }

        let store = Arc::try_unwrap(session.store)
            .map_err(|_| anyhow::anyhow!("the session store is still in use"))?
            .into_inner();
        let event_count = store.event_count();
        store.finalize()?;

        tracing::info!(id = %session.id, events = event_count, "recording stopped");
        Ok(FinishedSession { id: session.id, dir: session.dir })
    }
}

struct FinishedSession {
    id: String,
    dir: PathBuf,
}

/// Build the collectors a config enables.
///
/// A disabled source is never constructed, which is what keeps it from
/// triggering its macOS permission prompt.
fn build_collectors(config: &CaptureConfig) -> Vec<Box<dyn Collector>> {
    let mut collectors: Vec<Box<dyn Collector>> = Vec::new();
    if config.app_activity || config.window_titles || config.browser_urls {
        collectors.push(Box::new(ActiveWindowCollector::new(
            config.window_titles,
            config.browser_urls,
        )));
    }
    if config.clipboard {
        collectors.push(Box::new(ClipboardCollector::new()));
    }
    if config.screen_frames {
        collectors.push(Box::new(ScreenCollector::new()));
    }
    collectors
}

fn start_microphone(session: &mut Active, device: Option<&str>) -> MicrophoneState {
    let index = session.audio_segments.len() + 1;
    match MicrophoneRecorder::start(&session.dir.join("audio"), index, device) {
        Ok(recorder) => {
            session.microphone = Some(recorder);
            MicrophoneState::On { device: device.unwrap_or("default").to_string() }
        }
        Err(err) => {
            tracing::warn!("microphone unavailable: {err:#}");
            MicrophoneState::Error { message: format!("{err:#}") }
        }
    }
}

async fn stop_microphone(session: &mut Active) {
    if let Some(recorder) = session.microphone.take()
        && let Some(segment) = recorder.stop()
    {
        tracing::info!(ms = segment.duration_ms(), "narration segment saved");
        session.audio_segments.push(segment);
    }
    session.microphone_state = MicrophoneState::Off;
}

/// Record a microphone state change where the rest of the app can see it: the
/// `narrated` flag the library badge reads, and a `narration.state` event so
/// the timeline knows which stretches were spoken over.
async fn note_narration(session: &Active, on: bool) {
    let mut store = session.store.lock().await;
    if on {
        store.mark_narrated().ok();
    }
    store
        .append(EventInput::new("recorder", EventPayload::NarrationState { on }))
        .ok();
}

/// Finalize any recording that was interrupted rather than stopped.
///
/// A crash, a force-quit, or a dev hot-reload kills the process mid-recording:
/// the events are all on disk (that is the point of an append-only log) but
/// `stoppedAt` is never stamped and post-processing never runs, so the library
/// shows a session with no reconstruction and analysis has nothing to read.
///
/// Called once at startup. The stop time is taken from the last event rather
/// than from now, so an interrupted recording does not appear to have run until
/// whenever the app happened to be reopened.
pub fn recover_interrupted_sessions() -> Result<usize> {
    let root = skillrec_core::paths::sessions_root()?;
    if !root.exists() {
        return Ok(0);
    }

    let mut recovered = 0;
    for entry in std::fs::read_dir(&root)? {
        let dir = entry?.path();
        let Some(mut meta) = read_json::<SessionMeta>(&dir.join("session.json")) else {
            continue;
        };
        if meta.stopped_at.is_some() {
            continue;
        }

        let events = read_events(&dir.join("events.jsonl"));
        let last = events.last().map(|e| e.epoch).unwrap_or(meta.started_at);
        meta.stopped_at = Some(last);
        if let Err(err) = write_json(&dir.join("session.json"), &meta) {
            tracing::warn!(id = %meta.id, "could not finalize an interrupted recording: {err:#}");
            continue;
        }
        if let Err(err) = process_session(&dir) {
            tracing::warn!(id = %meta.id, "could not post-process a recovered recording: {err:#}");
            continue;
        }
        recovered += 1;
        tracing::info!(
            id = %meta.id,
            events = events.len(),
            "recovered a recording that was interrupted"
        );
    }
    Ok(recovered)
}

/// Post-stop processing: always produce `bundle.json` and `description.md`.
///
/// This is the model-free reconstruction. It runs whether or not an endpoint is
/// configured, so a recording is never just an opaque folder of JSON. The work
/// lives in core so a server can do exactly the same to a recording it receives.
pub fn process_session(dir: &std::path::Path) -> Result<()> {
    skillrec_core::session::reconstruct_session(dir)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use skillrec_capture::collector::CollectorContext;

    use super::*;

    /// `SKILLREC_DATA_DIR` is process-global, so tests that redirect it take
    /// this lock — otherwise one test's sessions show up in another's folder.
    static DATA_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct TempData {
        root: PathBuf,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl TempData {
        fn new(name: &str) -> Self {
            let guard = DATA_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let root =
                std::env::temp_dir().join(format!("skillrec-rec-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            unsafe { std::env::set_var("SKILLREC_DATA_DIR", &root) };
            Self { root, _guard: guard }
        }

        fn session_dir(&self, id: &str) -> PathBuf {
            self.root.join("sessions").join(id)
        }
    }

    impl Drop for TempData {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("SKILLREC_DATA_DIR") };
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// Publishes a marker on every tick, and one more after being told to stop
    /// — the way the screen sampler's final frame lands right at the boundary.
    struct Chatter;

    impl Collector for Chatter {
        fn name(&self) -> &'static str {
            "chatter"
        }

        fn run(&mut self, ctx: CollectorContext) {
            let mut index = 0;
            loop {
                ctx.publish("chatter", EventPayload::Marker { note: format!("tick {index}") });
                index += 1;
                if !ctx.sleep_or_stop(Duration::from_millis(1)) {
                    break;
                }
            }
            ctx.publish("chatter", EventPayload::Marker { note: "last".into() });
        }
    }

    fn sample_event() -> skillrec_core::events::RecEvent {
        skillrec_core::events::RecEvent {
            seq: 1,
            t: 5_000,
            epoch: 6_000,
            source: "test".into(),
            payload: EventPayload::AppActivate {
                app: "Safari".into(),
                title: "Pricing".into(),
                url: None,
                host: None,
                bundle_id: None,
                pid: None,
                bounds: None,
            },
        }
    }

    #[test]
    fn every_enabled_source_gets_a_collector() {
        let all = build_collectors(&CaptureConfig::default());
        let names: Vec<&str> = all.iter().map(|c| c.name()).collect();
        assert!(names.contains(&"active-window"));
        assert!(names.contains(&"clipboard"));
        assert!(names.contains(&"screen"));
    }

    #[test]
    fn a_disabled_source_is_never_constructed() {
        // Not merely inert: constructing it is what triggers the macOS prompt.
        let config = CaptureConfig {
            app_activity: false,
            window_titles: false,
            browser_urls: false,
            clipboard: false,
            screen_frames: false,
        };
        assert!(build_collectors(&config).is_empty());

        let clipboard_only = CaptureConfig { screen_frames: false, ..CaptureConfig::default() };
        let names: Vec<&str> = build_collectors(&clipboard_only).iter().map(|c| c.name()).collect();
        assert!(!names.contains(&"screen"));
        assert!(names.contains(&"clipboard"));
    }

    #[tokio::test]
    async fn an_idle_recorder_reports_no_session() {
        let recorder = Recorder::new("test");
        let status = recorder.status().await;
        assert!(!status.recording);
        assert!(status.session_id.is_none());
        assert_eq!(status.microphone, MicrophoneState::Off);
    }

    #[tokio::test]
    async fn stopping_when_idle_is_an_error_not_a_panic() {
        let recorder = Recorder::new("test");
        assert!(recorder.stop().await.is_err());
        assert!(recorder.discard().await.is_err());
        assert!(recorder.set_microphone(true, None).await.is_err());
    }

    #[tokio::test]
    async fn every_event_sent_before_stop_is_on_disk_and_the_stop_marker_is_last() {
        // Regression: an earlier version aborted the drain task on stop, which
        // could drop whatever a collector had sent in its final moments and let
        // the stop marker land before events that happened earlier.
        let data = TempData::new("drain");
        let recorder = Recorder::new("test");
        let id = recorder.start_with(vec![Box::new(Chatter)], false, None).await.unwrap();
        assert!(recorder.is_recording().await);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(recorder.stop().await.unwrap(), id);
        assert!(!recorder.is_recording().await);

        let dir = data.session_dir(&id);
        let events = read_events(&dir.join("events.jsonl"));
        assert_eq!(events.first().map(|e| e.kind()), Some("session.start"));
        assert_eq!(events.last().map(|e| e.kind()), Some("session.stop"));

        let markers: Vec<&str> = events
            .iter()
            .filter_map(|e| match &e.payload {
                EventPayload::Marker { note } => Some(note.as_str()),
                _ => None,
            })
            .collect();
        assert!(markers.len() > 2, "expected a stream of ticks, saw {}", markers.len());
        assert_eq!(markers.last(), Some(&"last"), "the collector's final event was dropped");
        for (index, note) in markers[..markers.len() - 1].iter().enumerate() {
            assert_eq!(*note, format!("tick {index}"), "a tick went missing mid-stream");
        }

        // stop() returns only once the reconstruction is on disk, so the UI
        // never opens a recording with no description.
        assert!(dir.join("bundle.json").exists());
        assert!(dir.join("description.md").exists());
        let meta: SessionMeta = read_json(&dir.join("session.json")).unwrap();
        assert!(meta.stopped_at.is_some());
        assert_eq!(recorder.status().await.last_session_id.as_deref(), Some(id.as_str()));
    }

    #[tokio::test]
    async fn a_second_start_is_refused_and_discard_removes_the_folder() {
        let data = TempData::new("discard");
        let recorder = Recorder::new("test");
        let id = recorder.start_with(Vec::new(), false, None).await.unwrap();
        assert!(recorder.start_with(Vec::new(), false, None).await.is_err());
        assert!(data.session_dir(&id).exists());

        assert_eq!(recorder.discard().await.unwrap(), id);
        assert!(!recorder.is_recording().await);
        assert!(!data.session_dir(&id).exists());
        assert!(recorder.status().await.last_session_id.is_none(), "a discard is not a save");
    }

    #[test]
    fn post_processing_writes_both_artifacts_from_events_alone() {
        let dir = std::env::temp_dir().join(format!("skillrec-proc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let meta = SessionMeta {
            id: "proc".into(),
            started_at: 1_000,
            stopped_at: Some(31_000),
            platform: "macos".into(),
            app_version: "test".into(),
            narrated: false,
            title: None,
            submitted: None,
        };
        write_json(&dir.join("session.json"), &meta).unwrap();
        std::fs::write(
            dir.join("events.jsonl"),
            format!("{}\n", serde_json::to_string(&sample_event()).unwrap()),
        )
        .unwrap();

        process_session(&dir).unwrap();
        assert!(dir.join("bundle.json").exists());
        let description = std::fs::read_to_string(dir.join("description.md")).unwrap();
        assert!(description.contains("Safari"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_interrupted_recording_is_finalized_on_the_next_start() {
        // Observed for real: a dev hot-reload killed the app mid-recording and
        // left a session with no stoppedAt and no reconstruction, which the
        // library then listed as a permanently blank entry.
        let data = TempData::new("recover");
        let dir = data.session_dir("interrupted");
        std::fs::create_dir_all(&dir).unwrap();

        let meta = SessionMeta {
            id: "interrupted".into(),
            started_at: 1_000,
            stopped_at: None, // killed before finalize
            platform: "macos".into(),
            app_version: "test".into(),
            narrated: false,
            title: None,
            submitted: None,
        };
        write_json(&dir.join("session.json"), &meta).unwrap();
        std::fs::write(
            dir.join("events.jsonl"),
            format!("{}\n", serde_json::to_string(&sample_event()).unwrap()),
        )
        .unwrap();

        assert_eq!(recover_interrupted_sessions().unwrap(), 1);

        let healed: SessionMeta = read_json(&dir.join("session.json")).unwrap();
        // Stopped at the last event, not at whenever the app was reopened.
        assert_eq!(healed.stopped_at, Some(6_000));
        assert!(dir.join("bundle.json").exists());
        assert!(dir.join("description.md").exists());

        // Idempotent: a second startup must not touch an already-closed session.
        assert_eq!(recover_interrupted_sessions().unwrap(), 0);
    }

    #[test]
    fn post_processing_a_folder_without_metadata_is_an_error() {
        let dir = std::env::temp_dir().join(format!("skillrec-noproc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(process_session(&dir).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
