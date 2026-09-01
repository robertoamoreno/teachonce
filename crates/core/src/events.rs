//! The formal event schema shared by producers (collectors) and consumers
//! (session store, timeline, describer).
//!
//! On disk each event is one JSON object per line in `events.jsonl`. The `type`
//! discriminant and its `payload` are adjacently tagged and flattened into the
//! envelope, so a line looks exactly like the TypeScript original:
//!
//! ```json
//! {"seq":7,"t":4210,"epoch":1712345678901,"source":"active-window",
//!  "type":"app.activate","payload":{"app":"Safari","title":"Pricing"}}
//! ```

use serde::{Deserialize, Serialize};

use crate::clock::{AtMs, EpochMs};

/// Window geometry, when the platform reports it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// The typed `type` → `payload` contract. Adding a variant here is the only way
/// to add a signal, which keeps collectors and the timeline in lockstep.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum EventPayload {
    #[serde(rename = "session.start")]
    SessionStart { platform: String },

    #[serde(rename = "session.stop")]
    SessionStop {},

    #[serde(rename = "marker")]
    Marker { note: String },

    /// The frontmost application changed.
    #[serde(rename = "app.activate")]
    AppActivate {
        app: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bundle_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bounds: Option<WindowBounds>,
    },

    /// Same app, new window/document title.
    #[serde(rename = "app.title-change")]
    AppTitleChange { app: String, title: String },

    /// The active browser tab navigated.
    #[serde(rename = "browser.url")]
    BrowserUrl {
        app: String,
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },

    /// Something was copied. Never the full contents — see `ClipboardCollector`.
    #[serde(rename = "clipboard.change")]
    ClipboardChange {
        formats: Vec<String>,
        length: usize,
        hash: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text_preview: Option<String>,
    },

    /// A screen still was kept (the screen changed, or a heartbeat came due).
    #[serde(rename = "frame.captured")]
    FrameCaptured {
        file: String,
        reason: FrameReason,
        phash: String,
        width: u32,
        height: u32,
    },

    /// Microphone capture started / stopped mid-session.
    #[serde(rename = "narration.state")]
    NarrationState { on: bool },
}

/// Why a screen still was retained. Purely diagnostic, but it lets the describer
/// tell "the screen actually changed here" from "nothing happened for 5s".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FrameReason {
    /// The perceptual hash moved past the dedupe threshold.
    Changed,
    /// Nothing changed, but the heartbeat interval elapsed.
    Heartbeat,
    /// First frame of the session.
    Initial,
}

/// An event a collector hands to the bus, before it is stamped and persisted.
#[derive(Debug, Clone)]
pub struct EventInput {
    pub source: &'static str,
    pub payload: EventPayload,
}

impl EventInput {
    pub fn new(source: &'static str, payload: EventPayload) -> Self {
        Self { source, payload }
    }
}

/// A persisted event: the collector's payload plus the sequencing the store adds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecEvent {
    /// Monotonic index within the session, starting at 1.
    pub seq: u64,
    /// Milliseconds since the recording started.
    pub t: AtMs,
    /// Wall-clock milliseconds.
    pub epoch: EpochMs,
    /// Which collector produced this.
    pub source: String,
    #[serde(flatten)]
    pub payload: EventPayload,
}

impl RecEvent {
    /// The dotted type name, matching the on-disk `type` field.
    pub fn kind(&self) -> &'static str {
        match self.payload {
            EventPayload::SessionStart { .. } => "session.start",
            EventPayload::SessionStop { .. } => "session.stop",
            EventPayload::Marker { .. } => "marker",
            EventPayload::AppActivate { .. } => "app.activate",
            EventPayload::AppTitleChange { .. } => "app.title-change",
            EventPayload::BrowserUrl { .. } => "browser.url",
            EventPayload::ClipboardChange { .. } => "clipboard.change",
            EventPayload::FrameCaptured { .. } => "frame.captured",
            EventPayload::NarrationState { .. } => "narration.state",
        }
    }

    /// Events that describe user intent, as opposed to recorder bookkeeping.
    ///
    /// These are what the timeline segments on and what frame correlation anchors
    /// to — session start/stop and the frame stills themselves are excluded
    /// precisely because they are *our* events, not the user's actions.
    pub fn is_meaningful(&self) -> bool {
        matches!(
            self.payload,
            EventPayload::AppActivate { .. }
                | EventPayload::AppTitleChange { .. }
                | EventPayload::BrowserUrl { .. }
                | EventPayload::ClipboardChange { .. }
                | EventPayload::Marker { .. }
        )
    }

    /// The app this event happened in, when it names one.
    pub fn app(&self) -> Option<&str> {
        match &self.payload {
            EventPayload::AppActivate { app, .. }
            | EventPayload::AppTitleChange { app, .. }
            | EventPayload::BrowserUrl { app, .. } => Some(app),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(payload: EventPayload) -> RecEvent {
        RecEvent { seq: 1, t: 0, epoch: 0, source: "test".into(), payload }
    }

    #[test]
    fn on_disk_shape_is_flat_type_and_payload() {
        let ev = event(EventPayload::AppTitleChange {
            app: "Safari".into(),
            title: "Pricing".into(),
        });
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["type"], "app.title-change");
        assert_eq!(json["payload"]["app"], "Safari");
        // The envelope stays flat — no nested "payload.payload".
        assert_eq!(json["seq"], 1);
    }

    #[test]
    fn events_round_trip_through_jsonl() {
        let ev = event(EventPayload::ClipboardChange {
            formats: vec!["text/plain".into()],
            length: 12,
            hash: "abc123".into(),
            text_preview: Some("hello".into()),
        });
        let line = serde_json::to_string(&ev).unwrap();
        let back: RecEvent = serde_json::from_str(&line).unwrap();
        assert_eq!(back.payload, ev.payload);
        assert_eq!(back.kind(), "clipboard.change");
    }

    #[test]
    fn recorder_bookkeeping_is_not_meaningful() {
        assert!(!event(EventPayload::SessionStart { platform: "macos".into() }).is_meaningful());
        assert!(!event(EventPayload::FrameCaptured {
            file: "f.jpg".into(),
            reason: FrameReason::Heartbeat,
            phash: "0".into(),
            width: 1,
            height: 1,
        })
        .is_meaningful());
        assert!(event(EventPayload::BrowserUrl {
            app: "Safari".into(),
            url: "https://example.com".into(),
            host: None,
            title: None,
        })
        .is_meaningful());
    }
}
