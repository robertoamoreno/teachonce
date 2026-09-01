//! The describer's output contract: one intent plus the ordered steps the user
//! actually took.
//!
//! This is the hand-off point between "what was recorded" and "what gets built".
//! The user reviews and edits it, so it is persisted as `analysis.json` and every
//! field is designed to be shown in a form: short title, one-sentence intent,
//! per-step confidence you can argue with.

use serde::{Deserialize, Serialize};

use crate::clock::AtMs;

/// How sure the model is. Kept coarse on purpose — a model's numeric confidence
/// is not calibrated, but "high / medium / low" is a useful sort key for the
/// reviewer's attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    #[default]
    Medium,
    Low,
}

/// One reconstructed action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisStep {
    /// Optional on the way in: models routinely omit it, and
    /// [`Analysis::from_submission`] assigns dense ids afterwards anyway.
    #[serde(default)]
    pub id: String,
    /// Past tense, addressed to the user: "Copied the enterprise tier price".
    pub title: String,
    /// One to three sentences on what happened and why it mattered.
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub start_ms: Option<AtMs>,
    #[serde(default)]
    pub end_ms: Option<AtMs>,
    #[serde(default)]
    pub apps: Vec<String>,
    /// Short references the model relied on — an event type, a URL, a frame file.
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub confidence: Confidence,
}

/// One round of user feedback, kept so the conversation is reconstructible.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackEntry {
    pub revision: u32,
    pub note: String,
    pub at: i64,
}

/// `analysis.json`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Analysis {
    pub session_id: String,
    /// 2–5 words, Title Case — the session's name in the library.
    pub title: String,
    /// One sentence naming the user's goal.
    pub intent: String,
    #[serde(default)]
    pub intent_confidence: Confidence,
    #[serde(default)]
    pub intent_rationale: String,
    pub steps: Vec<AnalysisStep>,
    /// Bumped on every re-analysis or edit.
    #[serde(default)]
    pub revision: u32,
    #[serde(default)]
    pub feedback_log: Vec<FeedbackEntry>,
    /// Which model produced this, so a re-run with a different endpoint is
    /// distinguishable from an edit.
    #[serde(default)]
    pub model: String,
}

/// What the model sends to the `submit_analysis` tool, before we attach
/// bookkeeping like the revision counter and model id.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisSubmission {
    pub title: String,
    pub intent: String,
    #[serde(default)]
    pub intent_confidence: Confidence,
    #[serde(default)]
    pub intent_rationale: String,
    #[serde(default)]
    pub steps: Vec<AnalysisStep>,
}

impl Analysis {
    /// Fold a submission into a persisted analysis, normalising what the model
    /// got loose about.
    ///
    /// Models reliably drift on two things: they hand back a title that is just
    /// the intent sentence truncated, and they renumber or skip step ids. Both
    /// are fixed here rather than nagged about in the prompt.
    pub fn from_submission(
        session_id: &str,
        model: &str,
        previous: Option<&Analysis>,
        submission: AnalysisSubmission,
    ) -> Self {
        let mut steps = submission.steps;
        for (index, step) in steps.iter_mut().enumerate() {
            if step.id.trim().is_empty() {
                step.id = format!("s{}", index + 1);
            }
            step.title = step.title.trim().to_string();
            step.detail = step.detail.trim().to_string();
        }

        Self {
            session_id: session_id.to_string(),
            title: normalize_title(&submission.title, &submission.intent),
            intent: submission.intent.trim().to_string(),
            intent_confidence: submission.intent_confidence,
            intent_rationale: submission.intent_rationale.trim().to_string(),
            steps,
            revision: previous.map(|p| p.revision + 1).unwrap_or(1),
            feedback_log: previous.map(|p| p.feedback_log.clone()).unwrap_or_default(),
            model: model.to_string(),
        }
    }

    /// Apply a direct user edit. Not a re-analysis: the model is not involved, so
    /// the revision advances but no feedback is logged.
    pub fn apply_edit(
        &mut self,
        title: Option<String>,
        intent: Option<String>,
        steps: Option<Vec<AnalysisStep>>,
    ) {
        if let Some(title) = title {
            self.title = title.trim().to_string();
        }
        // A blank intent is ignored: a session always has a goal, and clearing it
        // would leave the builders with nothing to generalise from.
        if let Some(intent) = intent.filter(|i| !i.trim().is_empty()) {
            self.intent = intent.trim().to_string();
        }
        if let Some(steps) = steps {
            self.steps = steps;
        }
        self.revision += 1;
    }

    pub fn log_feedback(&mut self, note: &str) {
        self.feedback_log.push(FeedbackEntry {
            revision: self.revision,
            note: note.trim().to_string(),
            at: crate::clock::epoch_ms(),
        });
    }

    /// Compact rendering handed to the builders as their input.
    pub fn render(&self) -> String {
        let mut out = format!("# {}\n\n**Intent:** {}\n", self.title, self.intent);
        if !self.intent_rationale.is_empty() {
            out.push_str(&format!("**Why:** {}\n", self.intent_rationale));
        }
        out.push_str("\n## Steps\n\n");
        for step in &self.steps {
            out.push_str(&format!("{}. **{}**\n", step.id, step.title));
            if !step.detail.is_empty() {
                out.push_str(&format!("   {}\n", step.detail));
            }
            if !step.apps.is_empty() {
                out.push_str(&format!("   _apps: {}_\n", step.apps.join(", ")));
            }
        }
        out
    }
}

const MAX_TITLE_CHARS: usize = 48;

/// Keep the title short and distinct from the intent sentence.
fn normalize_title(title: &str, intent: &str) -> String {
    let title = title.trim().trim_end_matches('.').trim();
    let fallback = || {
        intent
            .split_whitespace()
            .take(5)
            .collect::<Vec<_>>()
            .join(" ")
            .trim_end_matches(&[',', '.'][..])
            .to_string()
    };
    if title.is_empty() {
        return fallback();
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        let short: String = title.split_whitespace().take(5).collect::<Vec<_>>().join(" ");
        return short;
    }
    title.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn submission() -> AnalysisSubmission {
        AnalysisSubmission {
            title: "Research Habit Articles".into(),
            intent: "Research and compare articles on building better habits".into(),
            intent_confidence: Confidence::High,
            intent_rationale: "Navigated between two habit guides and copied a passage.".into(),
            steps: vec![AnalysisStep {
                id: String::new(),
                title: "  Opened the habits guide  ".into(),
                detail: " Read the intro. ".into(),
                start_ms: Some(0),
                end_ms: Some(4_000),
                apps: vec!["Safari".into()],
                evidence: vec!["browser.url".into()],
                confidence: Confidence::High,
            }],
        }
    }

    #[test]
    fn submissions_get_dense_step_ids_and_trimmed_text() {
        let a = Analysis::from_submission("sess", "gpt-4o", None, submission());
        assert_eq!(a.steps[0].id, "s1");
        assert_eq!(a.steps[0].title, "Opened the habits guide");
        assert_eq!(a.steps[0].detail, "Read the intro.");
        assert_eq!(a.revision, 1);
        assert_eq!(a.model, "gpt-4o");
    }

    #[test]
    fn re_analysis_advances_the_revision_and_keeps_the_feedback_log() {
        let mut first = Analysis::from_submission("sess", "m", None, submission());
        first.log_feedback("you missed a step");
        let second = Analysis::from_submission("sess", "m", Some(&first), submission());
        assert_eq!(second.revision, 2);
        assert_eq!(second.feedback_log.len(), 1);
    }

    #[test]
    fn an_overlong_title_is_shortened_rather_than_shown_truncated() {
        let long = AnalysisSubmission {
            title: "Copy the last few messages of a Teams chat into a brand new Apple Note".into(),
            ..submission()
        };
        let a = Analysis::from_submission("sess", "m", None, long);
        assert!(a.title.chars().count() <= MAX_TITLE_CHARS);
        assert_eq!(a.title.split_whitespace().count(), 5);
    }

    #[test]
    fn a_missing_title_falls_back_to_the_intent() {
        let blank = AnalysisSubmission { title: "   ".into(), ..submission() };
        let a = Analysis::from_submission("sess", "m", None, blank);
        assert_eq!(a.title, "Research and compare articles on");
    }

    #[test]
    fn editing_a_blank_intent_is_ignored_but_a_blank_title_is_allowed() {
        let mut a = Analysis::from_submission("sess", "m", None, submission());
        let intent = a.intent.clone();
        a.apply_edit(Some(String::new()), Some("   ".into()), None);
        assert_eq!(a.intent, intent, "a session always keeps a goal");
        assert_eq!(a.title, "");
        assert_eq!(a.revision, 2);
    }
}
