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
use skillrec_core::describe::render_description;
use skillrec_core::events::EventPayload;
use skillrec_core::narration::NarrationTranscript;
use skillrec_core::session::{read_events, read_json, write_json, SessionMeta, SessionStore};
use skillrec_core::timeline::build_bundle;
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
    pub async fn start(&self, config: CaptureConfig, narrate: bool) -> Result<String> {
        let mut active = self.active.lock().await;
        anyhow::ensure!(active.is_none(), "a recording is already running");

        let mut store = SessionStore::create(&self.app_version)?;
        let id = store.meta().id.clone();
        let dir = store.dir().to_path_buf();
        let started_at = store.meta().started_at;

        store
            .append(skillrec_core::events::EventInput::new(
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

        let host = CollectorHost::start(build_collectors(&config), tx, dir.clone(), started_at);
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
            session.microphone_state = start_microphone(&mut session, None);
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
                session.store.lock().await.mark_narrated().ok();
                session
                    .store
                    .lock()
                    .await
                    .append(skillrec_core::events::EventInput::new(
                        "recorder",
                        EventPayload::NarrationState { on: true },
                    ))
                    .ok();
            }
        } else {
            stop_microphone(session).await;
        }
        Ok(session.microphone_state.clone())
    }

    /// Stop and keep the recording. Returns the session id.
    pub async fn stop(&self) -> Result<String> {
        let session = self.finish().await?;
        let id = session.id.clone();
        let dir = session.dir.clone();

        // Post-processing is deterministic and fast, but it still runs off the
        // caller's path so the UI returns the moment capture has drained.
        tokio::task::spawn_blocking(move || {
            if let Err(err) = process_session(&dir) {
                tracing::warn!("post-processing failed: {err:#}");
            }
        });

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

    /// Shared teardown: stop producers, flush, finalize.
    async fn finish(&self) -> Result<FinishedSession> {
        let mut active = self.active.lock().await;
        let mut session = active.take().context("no recording is running")?;

        // Order matters. The microphone is flushed first so its stop boundary is
        // as close as possible to the click, then the collectors are joined so
        // the final frame and the frame manifest are on disk, and only then is
        // the event channel closed.
        stop_microphone(&mut session).await;
        session.host.stop();

        session
            .store
            .lock()
            .await
            .append(skillrec_core::events::EventInput::new(
                "recorder",
                EventPayload::SessionStop {},
            ))
            .ok();

        // Dropping the last sender ends the drain task, which has already
        // received everything the collectors sent before they were joined.
        session.drain.abort();
        let _ = session.drain.await;

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
/// configured, so a recording is never just an opaque folder of JSON.
pub fn process_session(dir: &std::path::Path) -> Result<()> {
    let meta: SessionMeta = read_json(&dir.join("session.json"))
        .context("this recording has no readable session.json")?;
    let events = read_events(&dir.join("events.jsonl"));
    let bundle = build_bundle(&meta, &events);
    let narration: Option<NarrationTranscript> = read_json(&dir.join("narration.json"));

    write_json(&dir.join("bundle.json"), &bundle).context("writing bundle.json")?;
    std::fs::write(dir.join("description.md"), render_description(&bundle, narration.as_ref()))
        .context("writing description.md")?;

    tracing::info!(
        steps = bundle.stats.step_count,
        events = bundle.stats.event_count,
        frames = bundle.stats.frame_count,
        "post-processing complete"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        };
        write_json(&dir.join("session.json"), &meta).unwrap();
        let event = skillrec_core::events::RecEvent {
            seq: 1,
            t: 0,
            epoch: 1_000,
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
        };
        std::fs::write(
            dir.join("events.jsonl"),
            format!("{}\n", serde_json::to_string(&event).unwrap()),
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
        let root = std::env::temp_dir().join(format!("skillrec-recover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        unsafe { std::env::set_var("SKILLREC_DATA_DIR", &root) };
        let dir = root.join("sessions").join("interrupted");
        std::fs::create_dir_all(&dir).unwrap();

        let meta = SessionMeta {
            id: "interrupted".into(),
            started_at: 1_000,
            stopped_at: None, // killed before finalize
            platform: "macos".into(),
            app_version: "test".into(),
            narrated: false,
            title: None,
        };
        write_json(&dir.join("session.json"), &meta).unwrap();
        let event = skillrec_core::events::RecEvent {
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
        };
        std::fs::write(
            dir.join("events.jsonl"),
            format!("{}\n", serde_json::to_string(&event).unwrap()),
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

        unsafe { std::env::remove_var("SKILLREC_DATA_DIR") };
        std::fs::remove_dir_all(&root).ok();
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
