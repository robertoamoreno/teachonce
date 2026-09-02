//! `description.md` — the model-free narrative of a recording.
//!
//! Every recording gets one the moment it stops. It is what the library shows
//! before you have analysed anything, what you fall back to when no endpoint is
//! configured, and — because it is compact and already structured — what the
//! describer's `get_timeline` tool hands the model as its opening context.

use crate::clock::format_span;
use crate::narration::NarrationTranscript;
use crate::timeline::Bundle;

/// Render the deterministic narrative.
pub fn render_description(bundle: &Bundle, narration: Option<&NarrationTranscript>) -> String {
    let mut out = String::new();
    out.push_str("# Recording\n\n");
    out.push_str(&format!(
        "{} steps over {} — {} events, {} frames.\n",
        bundle.stats.step_count,
        format_span(bundle.stats.duration_ms),
        bundle.stats.event_count,
        bundle.stats.frame_count
    ));
    if !bundle.stats.apps.is_empty() {
        out.push_str(&format!("Apps: {}.\n", bundle.stats.apps.join(", ")));
    }
    if !bundle.stats.hosts.is_empty() {
        out.push_str(&format!("Sites: {}.\n", bundle.stats.hosts.join(", ")));
    }

    if let Some(narration) = narration.filter(|n| !n.is_empty()) {
        out.push_str("\n## Narration\n\n");
        for segment in &narration.segments {
            out.push_str(&format!("- [{}] {}\n", format_span(segment.at_ms), segment.text));
        }
    }

    out.push_str("\n## Steps\n\n");
    if bundle.steps.is_empty() {
        out.push_str("_No steps were reconstructed from this recording._\n");
        return out;
    }

    for step in &bundle.steps {
        out.push_str(&format!(
            "### {} — {} ({} → {})\n\n",
            step.id,
            step.app,
            format_span(step.start_ms),
            format_span(step.end_ms)
        ));
        if !step.titles.is_empty() {
            out.push_str(&format!("- Titles: {}\n", step.titles.join(" → ")));
        }
        if !step.urls.is_empty() {
            out.push_str(&format!("- URLs: {}\n", step.urls.join(", ")));
        }
        for copied in &step.clipboard {
            out.push_str(&format!("- Copied: \"{copied}\"\n"));
        }
        for marker in &step.markers {
            out.push_str(&format!("- Marker: {marker}\n"));
        }
        if let Some(narration) = narration {
            let said = narration.during(step.start_ms, step.end_ms);
            if !said.is_empty() {
                let text: Vec<&str> = said.iter().map(|s| s.text.as_str()).collect();
                out.push_str(&format!("- Said: \"{}\"\n", text.join(" ")));
            }
        }
        if !step.frames.is_empty() {
            out.push_str(&format!("- Frames: {}\n", step.frames.len()));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::narration::NarrationSegment;
    use crate::session::SessionMeta;
    use crate::timeline::{build_bundle, Step};

    fn bundle_with(steps: Vec<Step>) -> Bundle {
        let meta = SessionMeta {
            id: "t".into(),
            started_at: 0,
            stopped_at: Some(30_000),
            platform: "macos".into(),
            app_version: "t".into(),
            narrated: false,
            title: None,
            submitted: None,
        };
        let mut bundle = build_bundle(&meta, &[]);
        bundle.stats.step_count = steps.len();
        bundle.steps = steps;
        bundle
    }

    #[test]
    fn a_step_renders_its_evidence() {
        let bundle = bundle_with(vec![Step {
            id: "s1".into(),
            app: "Safari".into(),
            start_ms: 0,
            end_ms: 5_000,
            urls: vec!["https://example.com/pricing".into()],
            clipboard: vec!["Enterprise $99".into()],
            ..Default::default()
        }]);
        let md = render_description(&bundle, None);
        assert!(md.contains("### s1 — Safari"));
        assert!(md.contains("https://example.com/pricing"));
        assert!(md.contains("Copied: \"Enterprise $99\""));
    }

    #[test]
    fn narration_is_folded_into_the_step_it_overlaps() {
        let bundle = bundle_with(vec![Step {
            id: "s1".into(),
            app: "Safari".into(),
            start_ms: 0,
            end_ms: 5_000,
            ..Default::default()
        }]);
        let narration = NarrationTranscript {
            model: "ggml-small".into(),
            language: "en".into(),
            segments: vec![
                NarrationSegment { at_ms: 1_000, end_ms: 3_000, text: "checking the price".into() },
                NarrationSegment { at_ms: 20_000, end_ms: 21_000, text: "much later".into() },
            ],
        };
        let md = render_description(&bundle, Some(&narration));
        assert!(md.contains("## Narration"));
        assert!(md.contains("Said: \"checking the price\""));
        assert!(!md.contains("Said: \"much later\""));
    }

    #[test]
    fn an_empty_recording_says_so_instead_of_rendering_nothing() {
        let md = render_description(&bundle_with(vec![]), None);
        assert!(md.contains("No steps were reconstructed"));
    }
}
