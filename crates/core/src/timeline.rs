//! The deterministic timeline: raw events → ordered steps.
//!
//! This runs the moment a recording stops, with no model involved. It matters
//! for three reasons: it is what the describer's `get_timeline` tool returns (so
//! the model starts from structure rather than 800 raw events), it is the
//! fallback narrative when no LLM is configured at all, and it is cheap enough
//! to recompute whenever the parsing rules improve.
//!
//! Segmentation boundary: **a new step starts when the user changes app, or
//! stays in a browser but moves to a different host.** Everything else — title
//! changes, copies, in-site navigation — enriches the current step. That single
//! rule reproduces how people actually describe their own work ("I looked at the
//! pricing page, then I put it in the sheet").

use serde::{Deserialize, Serialize};

use crate::clock::AtMs;
use crate::events::{EventPayload, RecEvent};
use crate::session::SessionMeta;

/// One coherent stretch of work in a single app / site.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    /// Stable within a bundle: `s1`, `s2`, …
    pub id: String,
    pub app: String,
    pub start_ms: AtMs,
    pub end_ms: AtMs,
    /// Distinct window/document titles seen, in order.
    pub titles: Vec<String>,
    /// Distinct URLs visited, in order.
    pub urls: Vec<String>,
    /// Distinct hosts, for the one-line summary.
    pub hosts: Vec<String>,
    /// Text previews of anything copied during this step.
    pub clipboard: Vec<String>,
    /// Explicit user markers.
    pub markers: Vec<String>,
    /// Sequence numbers of the events folded into this step, so the describer can
    /// jump from a step straight to its evidence.
    pub event_seqs: Vec<u64>,
    /// Frames retained during this step, as session-relative paths.
    pub frames: Vec<String>,
}

impl Step {
    pub fn duration_ms(&self) -> i64 {
        (self.end_ms - self.start_ms).max(0)
    }

    fn push_unique(list: &mut Vec<String>, value: &str) {
        let value = value.trim();
        if value.is_empty() {
            return;
        }
        if !list.iter().any(|existing| existing == value) {
            list.push(value.to_string());
        }
    }
}

/// Counts shown in the UI and in the describer's kickoff context.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleStats {
    pub step_count: usize,
    pub event_count: usize,
    pub meaningful_event_count: usize,
    pub frame_count: usize,
    pub duration_ms: i64,
    pub apps: Vec<String>,
    pub hosts: Vec<String>,
}

/// `bundle.json` — the deterministic reconstruction of a recording.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bundle {
    pub session_id: String,
    pub started_at: i64,
    pub steps: Vec<Step>,
    pub stats: BundleStats,
}

/// Strip the tracking parameters that carry no intent, so two URLs that differ
/// only by an ad click id are recognised as the same page.
pub fn normalize_url(raw: &str) -> String {
    let (base, query) = match raw.split_once('?') {
        Some(parts) => parts,
        None => return raw.trim_end_matches('#').to_string(),
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| {
            let key = pair.split('=').next().unwrap_or("").to_lowercase();
            !(key.starts_with("utm_")
                || matches!(
                    key.as_str(),
                    "gclid" | "gad_source" | "fbclid" | "msclkid" | "mc_eid" | "igshid" | "ref_src"
                ))
        })
        .collect();
    if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    }
}

/// Host part of a URL, without a scheme, port, or `www.`.
pub fn host_of(url: &str) -> Option<String> {
    let rest = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url);
    let host = rest
        .split(['/', '?', '#'])
        .next()?
        .split('@')
        .next_back()?
        .split(':')
        .next()?;
    if host.is_empty() {
        return None;
    }
    Some(host.trim_start_matches("www.").to_lowercase())
}

/// Our own app, which the user only touches to press Start and Stop.
///
/// The capture side already filters this out by pid, which is exact. This is the
/// backstop for recordings made before that fix, and for any build whose window
/// slips through — hence matching the bundled display name, the cargo binary
/// name, and the names the app shipped under before it was TeachOnce.
const RECORDER_APP_NAMES: &[&str] =
    &["teachonce", "teach once", "skill recorder", "skill-recorder", "skillrecorder"];

fn is_recorder_app(app: &str) -> bool {
    let app = app.trim().to_lowercase();
    RECORDER_APP_NAMES.contains(&app.as_str())
}

/// Focus flickers shorter than this, with nothing else in them, are window
/// manager noise rather than a step the user took.
const MIN_STEP_MS: i64 = 400;

/// Segment an event stream into ordered steps.
pub fn build_bundle(meta: &SessionMeta, events: &[RecEvent]) -> Bundle {
    let mut steps: Vec<Step> = Vec::new();
    let mut current: Option<Step> = None;
    let mut current_host: Option<String> = None;
    let mut frame_count = 0usize;
    let mut meaningful = 0usize;

    for event in events {
        if event.is_meaningful() {
            meaningful += 1;
        }

        // Frames attach to whichever step is open; they are evidence, not
        // boundaries.
        if let EventPayload::FrameCaptured { file, .. } = &event.payload {
            frame_count += 1;
            if let Some(step) = current.as_mut() {
                step.frames.push(file.clone());
            }
            continue;
        }

        let app = event.app().unwrap_or_default().to_string();
        let host = match &event.payload {
            EventPayload::BrowserUrl { url, host, .. } => {
                host.clone().or_else(|| host_of(url))
            }
            EventPayload::AppActivate { url: Some(url), host, .. } => {
                host.clone().or_else(|| host_of(url))
            }
            _ => None,
        };

        let starts_new_step = match (&current, &event.payload) {
            (None, EventPayload::AppActivate { .. } | EventPayload::BrowserUrl { .. }) => true,
            (Some(step), EventPayload::AppActivate { .. }) => step.app != app,
            (Some(step), EventPayload::BrowserUrl { .. }) => {
                // A host change splits — but only once the step *has* a host.
                // The first URL seen after switching into a browser belongs to
                // the step that switch opened; treating it as a change would
                // split every single browser visit into two steps.
                step.app != app
                    || matches!((&host, &current_host), (Some(new), Some(old)) if new != old)
            }
            _ => false,
        };

        if starts_new_step && !app.is_empty() {
            // A step runs until the next one begins, not until its own last
            // event. Otherwise a step whose only event is the app switch itself
            // has zero duration and gets filtered out as a focus flicker.
            if let Some(mut step) = current.take() {
                step.end_ms = step.end_ms.max(event.t);
                steps.push(step);
            }
            current_host = host.clone();
            current = Some(Step {
                id: String::new(), // assigned after filtering, so ids stay dense
                app: app.clone(),
                start_ms: event.t,
                end_ms: event.t,
                ..Default::default()
            });
        }

        let Some(step) = current.as_mut() else {
            continue;
        };
        step.end_ms = step.end_ms.max(event.t);
        step.event_seqs.push(event.seq);

        match &event.payload {
            EventPayload::AppActivate { title, url, .. } => {
                Step::push_unique(&mut step.titles, title);
                if let Some(url) = url {
                    let url = normalize_url(url);
                    if let Some(h) = host_of(&url) {
                        Step::push_unique(&mut step.hosts, &h);
                    }
                    Step::push_unique(&mut step.urls, &url);
                }
            }
            EventPayload::AppTitleChange { title, .. } => {
                Step::push_unique(&mut step.titles, title);
            }
            EventPayload::BrowserUrl { url, title, .. } => {
                let url = normalize_url(url);
                if let Some(h) = host_of(&url) {
                    Step::push_unique(&mut step.hosts, &h);
                    current_host = Some(h);
                }
                Step::push_unique(&mut step.urls, &url);
                if let Some(title) = title {
                    Step::push_unique(&mut step.titles, title);
                }
            }
            EventPayload::ClipboardChange { text_preview: Some(preview), .. } => {
                Step::push_unique(&mut step.clipboard, preview);
            }
            EventPayload::Marker { note } => Step::push_unique(&mut step.markers, note),
            _ => {}
        }
    }

    // The final step runs to the end of the recording — the user stayed where
    // they were until they pressed Stop.
    if let Some(mut step) = current.take() {
        step.end_ms = step.end_ms.max(meta.duration_ms());
        steps.push(step);
    }

    steps.retain(is_substantive);
    for (index, step) in steps.iter_mut().enumerate() {
        step.id = format!("s{}", index + 1);
    }

    let mut apps: Vec<String> = Vec::new();
    let mut hosts: Vec<String> = Vec::new();
    for step in &steps {
        Step::push_unique(&mut apps, &step.app);
        for host in &step.hosts {
            Step::push_unique(&mut hosts, host);
        }
    }

    Bundle {
        session_id: meta.id.clone(),
        started_at: meta.started_at,
        stats: BundleStats {
            step_count: steps.len(),
            event_count: events.len(),
            meaningful_event_count: meaningful,
            frame_count,
            duration_ms: meta.duration_ms(),
            apps,
            hosts,
        },
        steps,
    }
}

/// Drop steps that are pure recorder bracketing or momentary focus flicker.
///
/// The recorder's own window is never part of the user's task: they focus it to
/// press Start and Stop. Keeping those as steps sends every downstream analysis
/// off with two junk entries, so they are filtered here rather than left to the
/// model's judgement.
fn is_substantive(step: &Step) -> bool {
    if is_recorder_app(&step.app) {
        return false;
    }
    let carries_content = !step.urls.is_empty()
        || !step.clipboard.is_empty()
        || !step.markers.is_empty()
        || step.titles.len() > 1;
    carries_content || step.duration_ms() >= MIN_STEP_MS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventPayload;

    fn meta() -> SessionMeta {
        SessionMeta {
            id: "test".into(),
            started_at: 1_000,
            stopped_at: Some(61_000),
            platform: "macos".into(),
            app_version: "test".into(),
            narrated: false,
            title: None,
            submitted: None,
        }
    }

    struct Builder {
        events: Vec<RecEvent>,
    }

    impl Builder {
        fn new() -> Self {
            Self { events: Vec::new() }
        }

        fn at(mut self, t: AtMs, payload: EventPayload) -> Self {
            self.events.push(RecEvent {
                seq: self.events.len() as u64 + 1,
                t,
                epoch: 1_000 + t,
                source: "test".into(),
                payload,
            });
            self
        }

        fn activate(self, t: AtMs, app: &str, title: &str) -> Self {
            self.at(
                t,
                EventPayload::AppActivate {
                    app: app.into(),
                    title: title.into(),
                    url: None,
                    host: None,
                    bundle_id: None,
                    pid: None,
                    bounds: None,
                },
            )
        }

        fn url(self, t: AtMs, app: &str, url: &str) -> Self {
            self.at(
                t,
                EventPayload::BrowserUrl {
                    app: app.into(),
                    url: url.into(),
                    host: None,
                    title: None,
                },
            )
        }

        fn build(self) -> Bundle {
            build_bundle(&meta(), &self.events)
        }
    }

    #[test]
    fn switching_apps_opens_a_new_step() {
        let bundle = Builder::new()
            .activate(0, "Safari", "Pricing")
            .activate(5_000, "Numbers", "Budget")
            .activate(9_000, "Safari", "Pricing")
            .build();
        assert_eq!(bundle.stats.step_count, 3);
        assert_eq!(bundle.steps[0].app, "Safari");
        assert_eq!(bundle.steps[1].app, "Numbers");
        assert_eq!(bundle.steps[0].id, "s1");
        assert_eq!(bundle.steps[2].id, "s3");
    }

    #[test]
    fn navigating_within_one_site_stays_in_one_step() {
        let bundle = Builder::new()
            .activate(0, "Safari", "Docs")
            .url(1_000, "Safari", "https://example.com/a")
            .url(2_000, "Safari", "https://example.com/b")
            .build();
        assert_eq!(bundle.stats.step_count, 1);
        assert_eq!(bundle.steps[0].urls.len(), 2);
        assert_eq!(bundle.steps[0].hosts, vec!["example.com"]);
    }

    #[test]
    fn moving_to_another_host_opens_a_new_step() {
        let bundle = Builder::new()
            .activate(0, "Safari", "Docs")
            .url(1_000, "Safari", "https://example.com/a")
            .url(4_000, "Safari", "https://other.com/x")
            .build();
        assert_eq!(bundle.stats.step_count, 2);
        assert_eq!(bundle.steps[1].hosts, vec!["other.com"]);
    }

    #[test]
    fn the_recorders_own_window_never_becomes_a_step() {
        let bundle = Builder::new()
            .activate(0, "Skill Recorder", "Skill Recorder")
            .activate(1_000, "Safari", "Pricing")
            .url(2_000, "Safari", "https://example.com/pricing")
            .activate(30_000, "Skill Recorder", "Skill Recorder")
            .build();
        assert_eq!(bundle.stats.step_count, 1);
        assert_eq!(bundle.steps[0].app, "Safari");
    }

    #[test]
    fn the_recorder_is_recognised_under_every_name_it_ships_under() {
        // Observed in a real 78-minute recording: the `cargo run` build reports
        // "skill-recorder", so a filter matching only the bundled display name
        // left a trailing "went back to press Stop" step in the reconstruction.
        for name in ["TeachOnce", "teachonce", "Skill Recorder", "skill-recorder", "SkillRecorder", " skill recorder "] {
            let bundle = Builder::new()
                .activate(0, "Safari", "Pricing")
                .url(500, "Safari", "https://example.com/pricing")
                .activate(30_000, name, "Skill Recorder")
                .build();
            assert_eq!(bundle.stats.step_count, 1, "{name:?} must not become a step");
            assert_eq!(bundle.steps[0].app, "Safari");
        }
        // A real app whose name merely contains "recorder" is not us.
        assert!(!is_recorder_app("Screen Recorder"));
        assert!(!is_recorder_app("QuickTime Player"));
    }

    #[test]
    fn a_sub_second_empty_flicker_is_dropped_but_a_copy_is_never_lost() {
        let flicker = Builder::new()
            .activate(0, "Safari", "Docs")
            .url(100, "Safari", "https://example.com")
            .activate(1_000, "Finder", "Desktop")
            .activate(1_100, "Safari", "Docs")
            .url(1_200, "Safari", "https://example.com")
            .build();
        assert_eq!(flicker.stats.step_count, 2, "the 100ms Finder blip is noise");

        let with_copy = Builder::new()
            .activate(0, "Safari", "Docs")
            .url(100, "Safari", "https://example.com")
            .activate(1_000, "Finder", "Desktop")
            .at(
                1_050,
                EventPayload::ClipboardChange {
                    formats: vec!["text/plain".into()],
                    length: 4,
                    hash: "h".into(),
                    text_preview: Some("acme".into()),
                },
            )
            .activate(1_100, "Safari", "Docs")
            .build();
        assert_eq!(with_copy.stats.step_count, 3, "a step that copied something is real");
        assert_eq!(with_copy.steps[1].clipboard, vec!["acme"]);
    }

    #[test]
    fn frames_attach_to_the_open_step_without_splitting_it() {
        let bundle = Builder::new()
            .activate(0, "Safari", "Docs")
            .at(
                500,
                EventPayload::FrameCaptured {
                    file: "frames/f1.jpg".into(),
                    reason: crate::events::FrameReason::Changed,
                    phash: "0".repeat(16),
                    width: 1280,
                    height: 720,
                },
            )
            .activate(2_000, "Numbers", "Budget")
            .build();
        assert_eq!(bundle.stats.step_count, 2);
        assert_eq!(bundle.steps[0].frames, vec!["frames/f1.jpg"]);
        assert_eq!(bundle.stats.frame_count, 1);
    }

    #[test]
    fn tracking_parameters_are_stripped_so_one_page_is_one_page() {
        assert_eq!(
            normalize_url("https://example.com/p?utm_source=x&id=7&gclid=abc"),
            "https://example.com/p?id=7"
        );
        assert_eq!(normalize_url("https://example.com/p?utm_source=x"), "https://example.com/p");
        assert_eq!(normalize_url("https://example.com/p"), "https://example.com/p");

        // Two links that differ only by ad tracking must land in one step.
        let bundle = Builder::new()
            .activate(0, "Safari", "Docs")
            .url(1_000, "Safari", "https://example.com/p?utm_source=news")
            .url(2_000, "Safari", "https://example.com/p?gclid=xyz")
            .build();
        assert_eq!(bundle.steps[0].urls.len(), 1);
    }

    #[test]
    fn hosts_normalize_away_scheme_port_and_www() {
        assert_eq!(host_of("https://www.Example.com:8443/a?b=c").as_deref(), Some("example.com"));
        assert_eq!(host_of("http://localhost:1234/v1").as_deref(), Some("localhost"));
        assert_eq!(host_of("not a url"), Some("not a url".into()));
        assert_eq!(host_of(""), None);
    }

    #[test]
    fn an_empty_recording_produces_an_empty_bundle_not_a_panic() {
        let bundle = build_bundle(&meta(), &[]);
        assert_eq!(bundle.stats.step_count, 0);
        assert_eq!(bundle.stats.duration_ms, 60_000);
    }
}
