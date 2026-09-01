//! The narration transcript: the user's own words, timestamped onto the session
//! clock.
//!
//! When present this is the single most direct statement of intent in a
//! recording — everything else is inference from apps and URLs. So the describer
//! is told to read it early and let it lead, and analysis refuses to run on a
//! session whose audio exists but has not been transcribed yet.

use serde::{Deserialize, Serialize};

use crate::clock::AtMs;

/// One utterance, on the session clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NarrationSegment {
    pub at_ms: AtMs,
    pub end_ms: AtMs,
    pub text: String,
}

/// `narration.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NarrationTranscript {
    /// Which checkpoint produced this, so a re-transcription with a better model
    /// is detectable.
    pub model: String,
    pub language: String,
    pub segments: Vec<NarrationSegment>,
}

impl NarrationTranscript {
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// Render as timestamped lines for the describer's `get_narration` tool.
    pub fn render(&self) -> String {
        self.segments
            .iter()
            .map(|s| format!("[{}ms] {}", s.at_ms, s.text))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Case-insensitive substring search, so the model can grep its own context
    /// instead of pulling the whole transcript into every turn.
    pub fn grep(&self, query: &str) -> Vec<&NarrationSegment> {
        let needle = query.to_lowercase();
        self.segments
            .iter()
            .filter(|s| s.text.to_lowercase().contains(&needle))
            .collect()
    }

    /// Segments overlapping a time window — how a timeline step picks up the
    /// words the user said while performing it.
    pub fn during(&self, from_ms: AtMs, to_ms: AtMs) -> Vec<&NarrationSegment> {
        self.segments
            .iter()
            .filter(|s| s.end_ms >= from_ms && s.at_ms <= to_ms)
            .collect()
    }
}

/// Whisper hallucinates confidently over silence, and it hallucinates the same
/// handful of phrases: the training data's subtitle boilerplate. Dropping these
/// is the difference between a transcript that reads as narration and one
/// sprinkled with "Thanks for watching!".
const BOILERPLATE: &[&str] = &[
    "thanks for watching",
    "thank you for watching",
    "subscribe",
    "see you next time",
    "you",
    "bye",
    "silence",
    "music",
    "applause",
    "blank_audio",
];

/// Is this chunk real speech, or a hallucination over silence?
pub fn is_meaningful_text(text: &str) -> bool {
    let cleaned = text
        .trim()
        .trim_matches(|c: char| c == '.' || c == '!' || c == '?' || c == '[' || c == ']' || c == '*')
        .trim()
        .to_lowercase();
    if cleaned.len() < 2 {
        return false;
    }
    if !cleaned.chars().any(|c| c.is_alphanumeric()) {
        return false;
    }
    !BOILERPLATE.iter().any(|b| cleaned == *b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcript() -> NarrationTranscript {
        NarrationTranscript {
            model: "ggml-small".into(),
            language: "en".into(),
            segments: vec![
                NarrationSegment { at_ms: 0, end_ms: 2_000, text: "Opening the pricing page".into() },
                NarrationSegment { at_ms: 3_000, end_ms: 6_000, text: "Copying the enterprise tier".into() },
                NarrationSegment { at_ms: 9_000, end_ms: 11_000, text: "Pasting it into the sheet".into() },
            ],
        }
    }

    #[test]
    fn rendering_puts_the_session_clock_in_front() {
        assert_eq!(
            transcript().render().lines().next().unwrap(),
            "[0ms] Opening the pricing page"
        );
    }

    #[test]
    fn grep_is_case_insensitive() {
        let t = transcript();
        assert_eq!(t.grep("COPYING").len(), 1);
        assert!(t.grep("nothing here").is_empty());
    }

    #[test]
    fn during_includes_partially_overlapping_speech() {
        let t = transcript();
        // A segment that starts before the window but runs into it is speech
        // said *while* the step was happening, so it counts.
        assert_eq!(t.during(1_000, 4_000).len(), 2);
        assert_eq!(t.during(20_000, 30_000).len(), 0);
    }

    #[test]
    fn silence_hallucinations_are_rejected() {
        assert!(!is_meaningful_text("Thanks for watching!"));
        assert!(!is_meaningful_text(" [BLANK_AUDIO] "));
        assert!(!is_meaningful_text("..."));
        assert!(!is_meaningful_text("*"));
        assert!(!is_meaningful_text(""));
    }

    #[test]
    fn real_speech_survives() {
        assert!(is_meaningful_text("Now I open the invoice"));
        // Short but real, and containing a boilerplate word without *being* it.
        assert!(is_meaningful_text("subscribe to the plan"));
    }
}
