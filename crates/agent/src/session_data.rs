//! Everything the tools read, loaded once per turn.
//!
//! Loading up front rather than per tool call keeps a turn consistent — the
//! model cannot see a timeline built from one state of the folder and events
//! from another — and it means a tool call costs no disk I/O, which matters when
//! a chatty local model makes twenty of them.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use skillrec_core::analysis::Analysis;
use skillrec_core::events::RecEvent;
use skillrec_core::frames::FrameManifest;
use skillrec_core::narration::NarrationTranscript;
use skillrec_core::session::{read_events, read_json, SessionMeta};
use skillrec_core::timeline::{build_bundle, Bundle};

/// A recording, as the tools see it.
pub struct SessionData {
    pub id: String,
    pub dir: PathBuf,
    pub meta: SessionMeta,
    pub events: Vec<RecEvent>,
    pub bundle: Bundle,
    pub narration: Option<NarrationTranscript>,
    pub frames: FrameManifest,
    pub analysis: Option<Analysis>,
}

impl SessionData {
    /// Load everything for one recording.
    pub fn load(dir: &Path) -> Result<Self> {
        let meta: SessionMeta = read_json(&dir.join("session.json"))
            .with_context(|| format!("{} has no readable session.json", dir.display()))?;
        let events = read_events(&dir.join("events.jsonl"));
        let bundle = build_bundle(&meta, &events);
        Ok(Self {
            id: meta.id.clone(),
            dir: dir.to_path_buf(),
            events,
            bundle,
            narration: read_json(&dir.join("narration.json")),
            frames: read_json(&dir.join("frames.json")).unwrap_or_default(),
            analysis: read_json(&dir.join("analysis.json")),
            meta,
        })
    }

    /// Read a frame's JPEG bytes, refusing any path that leaves the folder.
    pub fn read_frame(&self, relative: &str) -> Result<Vec<u8>> {
        let path = skillrec_core::paths::resolve_within(&self.dir, relative)?;
        std::fs::read(&path).with_context(|| format!("reading {}", path.display()))
    }
}

/// The timeline as the model sees it: compact, and on the session clock.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineView {
    pub duration_ms: i64,
    pub step_count: usize,
    pub frame_count: usize,
    pub narrated: bool,
    pub apps: Vec<String>,
    pub hosts: Vec<String>,
    pub steps: Vec<TimelineStepView>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineStepView {
    pub id: String,
    pub app: String,
    pub start_ms: i64,
    pub end_ms: i64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub titles: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub copied: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub said: Vec<String>,
    pub frame_count: usize,
}

impl SessionData {
    pub fn timeline_view(&self) -> TimelineView {
        let steps = self
            .bundle
            .steps
            .iter()
            .map(|step| TimelineStepView {
                id: step.id.clone(),
                app: step.app.clone(),
                start_ms: step.start_ms,
                end_ms: step.end_ms,
                titles: step.titles.clone(),
                urls: step.urls.clone(),
                copied: step.clipboard.clone(),
                said: self
                    .narration
                    .as_ref()
                    .map(|n| {
                        n.during(step.start_ms, step.end_ms)
                            .into_iter()
                            .map(|s| s.text.clone())
                            .collect()
                    })
                    .unwrap_or_default(),
                frame_count: step.frames.len(),
            })
            .collect();

        TimelineView {
            duration_ms: self.bundle.stats.duration_ms,
            step_count: self.bundle.stats.step_count,
            frame_count: self.frames.frames.len(),
            narrated: self.narration.as_ref().is_some_and(|n| !n.is_empty()),
            apps: self.bundle.stats.apps.clone(),
            hosts: self.bundle.stats.hosts.clone(),
            steps,
        }
    }

    /// Raw events in a window, optionally filtered by type.
    ///
    /// Capped: a chatty session can hold thousands of events, and handing all of
    /// them to a small model wastes its entire context on data it did not ask
    /// for. The reply says when it truncated so the model can narrow its window.
    pub fn events_view(
        &self,
        types: &[String],
        from_ms: Option<i64>,
        to_ms: Option<i64>,
        max: usize,
    ) -> serde_json::Value {
        let matching: Vec<&RecEvent> = self
            .events
            .iter()
            .filter(|event| {
                from_ms.is_none_or(|from| event.t >= from)
                    && to_ms.is_none_or(|to| event.t <= to)
                    && (types.is_empty() || types.iter().any(|t| t == event.kind()))
            })
            .collect();

        let total = matching.len();
        let truncated = total > max;
        let shown: Vec<serde_json::Value> = matching
            .into_iter()
            .take(max)
            .map(|event| {
                serde_json::json!({
                    "seq": event.seq,
                    "atMs": event.t,
                    "type": event.kind(),
                    "payload": serde_json::to_value(&event.payload)
                        .ok()
                        .and_then(|v| v.get("payload").cloned())
                        .unwrap_or(serde_json::Value::Null),
                })
            })
            .collect();

        serde_json::json!({
            "total": total,
            "shown": shown.len(),
            "truncated": truncated,
            "events": shown,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skillrec_core::events::EventPayload;
    use skillrec_core::narration::{NarrationSegment, NarrationTranscript};
    use skillrec_core::session::write_json;

    fn write_session(events: &[(i64, EventPayload)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("skillrec-agent-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let meta = SessionMeta {
            id: "testsession".into(),
            started_at: 1_000,
            stopped_at: Some(61_000),
            platform: "macos".into(),
            app_version: "test".into(),
            narrated: false,
            title: None,
            submitted: None,
        };
        write_json(&dir.join("session.json"), &meta).unwrap();
        let lines: String = events
            .iter()
            .enumerate()
            .map(|(index, (t, payload))| {
                let event = RecEvent {
                    seq: index as u64 + 1,
                    t: *t,
                    epoch: 1_000 + t,
                    source: "test".into(),
                    payload: payload.clone(),
                };
                format!("{}\n", serde_json::to_string(&event).unwrap())
            })
            .collect();
        std::fs::write(dir.join("events.jsonl"), lines).unwrap();
        dir
    }

    fn activate(app: &str) -> EventPayload {
        EventPayload::AppActivate {
            app: app.into(),
            title: format!("{app} window"),
            url: None,
            host: None,
            bundle_id: None,
            pid: None,
            bounds: None,
        }
    }

    #[test]
    fn a_session_loads_even_with_no_optional_artifacts() {
        let dir = write_session(&[(0, activate("Safari")), (5_000, activate("Numbers"))]);
        let data = SessionData::load(&dir).unwrap();
        assert_eq!(data.events.len(), 2);
        assert_eq!(data.bundle.steps.len(), 2);
        assert!(data.narration.is_none());
        assert!(data.frames.frames.is_empty());
        assert!(data.analysis.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_folder_without_metadata_is_an_error_not_an_empty_session() {
        let dir = std::env::temp_dir().join(format!("skillrec-empty-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(SessionData::load(&dir).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_timeline_view_folds_narration_into_the_step_it_overlaps() {
        let dir = write_session(&[(0, activate("Safari")), (10_000, activate("Numbers"))]);
        let mut data = SessionData::load(&dir).unwrap();
        data.narration = Some(NarrationTranscript {
            model: "ggml-small".into(),
            language: "en".into(),
            segments: vec![NarrationSegment {
                at_ms: 2_000,
                end_ms: 4_000,
                text: "checking the price".into(),
            }],
        });
        let view = data.timeline_view();
        assert!(view.narrated);
        assert_eq!(view.steps[0].said, vec!["checking the price"]);
        assert!(view.steps[1].said.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn events_can_be_filtered_by_window_and_type() {
        let dir = write_session(&[
            (0, activate("Safari")),
            (5_000, activate("Numbers")),
            (
                6_000,
                EventPayload::ClipboardChange {
                    formats: vec!["text/plain".into()],
                    length: 4,
                    hash: "h".into(),
                    text_preview: Some("acme".into()),
                },
            ),
        ]);
        let data = SessionData::load(&dir).unwrap();

        let window = data.events_view(&[], Some(4_000), Some(7_000), 100);
        assert_eq!(window["total"], 2);

        let filtered = data.events_view(&["clipboard.change".into()], None, None, 100);
        assert_eq!(filtered["total"], 1);
        assert_eq!(filtered["events"][0]["payload"]["text_preview"], "acme");
        assert_eq!(filtered["events"][0]["atMs"], 6_000);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn oversized_event_queries_are_truncated_and_say_so() {
        let events: Vec<(i64, EventPayload)> =
            (0..50).map(|i| (i * 100, activate(&format!("App{i}")))).collect();
        let dir = write_session(&events);
        let data = SessionData::load(&dir).unwrap();

        let view = data.events_view(&[], None, None, 10);
        assert_eq!(view["total"], 50);
        assert_eq!(view["shown"], 10);
        assert_eq!(view["truncated"], true);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn frame_reads_cannot_escape_the_session_folder() {
        let dir = write_session(&[(0, activate("Safari"))]);
        let data = SessionData::load(&dir).unwrap();
        // A tampered manifest must not turn into an arbitrary file read.
        assert!(data.read_frame("../../../etc/passwd").is_err());
        assert!(data.read_frame("/etc/passwd").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
