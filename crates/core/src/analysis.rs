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
    /// The pages open during this step: exact addresses stamped from the
    /// events by time (see [`crate::pages`]), never written by a model.
    #[serde(default)]
    pub urls: Vec<String>,
    #[serde(default)]
    pub confidence: Confidence,
}

/// What a debrief question is probing for.
///
/// Coarse on purpose: it labels the question in the UI and tells the builder
/// which part of the skill an answer belongs to. The kinds come from what
/// process interviewers and programming-by-demonstration systems have found
/// worth asking: conditionals, parameters, and the specifics nobody explains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QuestionKind {
    /// What happens off the happy path: a failure, a missing item, an odd case.
    #[default]
    Exception,
    /// Why one option was chosen over another, and what would change that.
    Decision,
    /// What differs from run to run: the inputs a skill must ask for or find.
    Variable,
    /// What must already be true: access, setup, an open file.
    Precondition,
    /// How the user knows the task is done, and what the result must look like.
    Outcome,
    /// An environment-specific fact an agent would get wrong without being told.
    Gotcha,
}

impl QuestionKind {
    /// Parse loosely. Models spell these many ways, and a misfiled question is
    /// still worth asking, so anything unrecognised lands on `Exception`.
    pub fn parse(raw: &str) -> Self {
        let raw = raw.trim().to_lowercase();
        let starts = |prefixes: &[&str]| prefixes.iter().any(|p| raw.starts_with(p));
        if starts(&["decision", "choice", "why", "judg"]) {
            Self::Decision
        } else if starts(&["var", "input", "param", "argument"]) {
            Self::Variable
        } else if starts(&["pre", "setup", "access", "require"]) {
            Self::Precondition
        } else if starts(&["out", "done", "result", "success", "complet"]) {
            Self::Outcome
        } else if starts(&["gotcha", "quirk", "caveat", "specific"]) {
            Self::Gotcha
        } else {
            Self::Exception
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Exception => "exception",
            Self::Decision => "decision",
            Self::Variable => "variable",
            Self::Precondition => "precondition",
            Self::Outcome => "outcome",
            Self::Gotcha => "gotcha",
        }
    }
}

/// One debrief question and, once the user has replied, its answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebriefQuestion {
    /// `q1`, `q2`, … assigned when the set is stored.
    #[serde(default)]
    pub id: String,
    pub question: String,
    /// What in the recording prompted it, so the user can see it is not generic.
    #[serde(default)]
    pub why: String,
    #[serde(default)]
    pub kind: QuestionKind,
    /// The analysis step it is about, when it is about one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    /// The user chose not to answer. It stays on record and is not asked again.
    #[serde(default)]
    pub skipped: bool,
}

impl DebriefQuestion {
    pub fn is_answered(&self) -> bool {
        self.answer.as_deref().is_some_and(|a| !a.trim().is_empty())
    }

    /// Still waiting on the user.
    pub fn is_open(&self) -> bool {
        !self.is_answered() && !self.skipped
    }
}

/// Ceiling on one round of questions. Five is what a person will answer in one
/// sitting; past that the debrief becomes a form nobody fills in.
pub const MAX_DEBRIEF_QUESTIONS: usize = 5;

/// One reply from the user, by question id.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebriefAnswer {
    pub id: String,
    #[serde(default)]
    pub answer: Option<String>,
    #[serde(default)]
    pub skipped: bool,
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
    /// The debrief: questions the recording could not answer, and what the
    /// user said. This is the decision layer a demonstration lacks, and the
    /// builder treats the answers as facts about the task.
    #[serde(default)]
    pub debrief: Vec<DebriefQuestion>,
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
            // The user's answers are knowledge about the task, not about one
            // model's reading of it. They survive a re-analysis.
            debrief: previous.map(|p| p.debrief.clone()).unwrap_or_default(),
        }
    }

    /// Replace the open questions with a fresh round, keeping everything the
    /// user already answered or skipped. At most [`MAX_DEBRIEF_QUESTIONS`] new
    /// ones are taken; ids are reassigned densely. Returns how many were added.
    pub fn set_open_questions(&mut self, fresh: Vec<DebriefQuestion>) -> usize {
        let mut kept: Vec<DebriefQuestion> =
            self.debrief.drain(..).filter(|q| !q.is_open()).collect();
        let mut added = 0;
        for mut question in fresh {
            question.question = question.question.trim().to_string();
            question.why = question.why.trim().to_string();
            question.step_id = question.step_id.filter(|s| !s.trim().is_empty());
            if question.question.is_empty() {
                continue;
            }
            // A question the user has already dealt with is not asked twice.
            if kept.iter().any(|k| k.question.eq_ignore_ascii_case(&question.question)) {
                continue;
            }
            question.answer = None;
            question.skipped = false;
            kept.push(question);
            added += 1;
            if added == MAX_DEBRIEF_QUESTIONS {
                break;
            }
        }
        for (index, question) in kept.iter_mut().enumerate() {
            question.id = format!("q{}", index + 1);
        }
        self.debrief = kept;
        added
    }

    /// Record the user's replies. Advances the revision when anything changed:
    /// the answers are part of what the builder works from.
    pub fn answer_debrief(&mut self, answers: &[DebriefAnswer]) -> usize {
        let mut changed = 0;
        for reply in answers {
            let Some(question) = self.debrief.iter_mut().find(|q| q.id == reply.id) else {
                continue;
            };
            let answer = reply
                .answer
                .as_deref()
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .map(str::to_string);
            // An answer beats a skip: if the user wrote something, keep it.
            let skipped = reply.skipped && answer.is_none();
            if question.answer != answer || question.skipped != skipped {
                question.answer = answer;
                question.skipped = skipped;
                changed += 1;
            }
        }
        if changed > 0 {
            self.revision += 1;
        }
        changed
    }

    /// Questions still waiting on the user.
    pub fn open_questions(&self) -> usize {
        self.debrief.iter().filter(|q| q.is_open()).count()
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
            if !step.urls.is_empty() {
                out.push_str(&format!("   pages: {}\n", step.urls.join(" ; ")));
            }
        }

        let answered: Vec<&DebriefQuestion> =
            self.debrief.iter().filter(|q| q.is_answered()).collect();
        if !answered.is_empty() {
            out.push_str(
                "\n## Debrief\n\nThe user answered these questions about the recording. Treat \
                 the answers as authoritative: they are the exceptions, decisions and inputs \
                 the recording alone could not show.\n\n",
            );
            for question in answered {
                let about = question
                    .step_id
                    .as_deref()
                    .map(|s| format!(", about {s}"))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "- ({}{}) {}\n  Answer: {}\n",
                    question.kind.label(),
                    about,
                    question.question,
                    question.answer.as_deref().unwrap_or_default()
                ));
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
                urls: vec![],
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

    fn question(text: &str, kind: QuestionKind) -> DebriefQuestion {
        DebriefQuestion {
            id: String::new(),
            question: text.into(),
            why: "seen in s1".into(),
            kind,
            step_id: Some("s1".into()),
            answer: None,
            skipped: false,
        }
    }

    #[test]
    fn a_round_of_questions_is_capped_numbered_and_deduplicated() {
        let mut a = Analysis::from_submission("sess", "m", None, submission());
        let fresh: Vec<DebriefQuestion> = (1..=7)
            .map(|i| question(&format!("Question {i}?"), QuestionKind::Exception))
            .chain([question("   ", QuestionKind::Decision)])
            .collect();
        assert_eq!(a.set_open_questions(fresh), MAX_DEBRIEF_QUESTIONS);
        assert_eq!(a.debrief.len(), MAX_DEBRIEF_QUESTIONS);
        assert_eq!(a.debrief[0].id, "q1");
        assert_eq!(a.debrief[4].id, "q5");
        assert_eq!(a.open_questions(), 5);
        // Generating questions is not a user edit, so the revision holds.
        assert_eq!(a.revision, 1);
    }

    #[test]
    fn answers_are_kept_across_a_new_round_and_a_re_analysis() {
        let mut a = Analysis::from_submission("sess", "m", None, submission());
        a.set_open_questions(vec![
            question("What if the page is empty?", QuestionKind::Exception),
            question("Why that guide first?", QuestionKind::Decision),
        ]);
        let changed = a.answer_debrief(&[
            DebriefAnswer { id: "q1".into(), answer: Some("  I stop and report it. ".into()), skipped: false },
            DebriefAnswer { id: "nope".into(), answer: Some("ignored".into()), skipped: false },
        ]);
        assert_eq!(changed, 1);
        assert_eq!(a.revision, 2, "an answer is part of the analysis");
        assert_eq!(a.debrief[0].answer.as_deref(), Some("I stop and report it."));
        assert_eq!(a.open_questions(), 1);

        // A fresh round replaces only the open question; the answered one and
        // an exact repeat of it are both left alone.
        let added = a.set_open_questions(vec![
            question("what if the page is empty?", QuestionKind::Exception),
            question("Which browser must be open?", QuestionKind::Precondition),
        ]);
        assert_eq!(added, 1);
        assert_eq!(a.debrief.len(), 2);
        assert!(a.debrief[0].is_answered());
        assert_eq!(a.debrief[1].question, "Which browser must be open?");
        assert_eq!(a.debrief[1].id, "q2");

        // Re-analysis keeps the debrief, like the feedback log.
        let again = Analysis::from_submission("sess", "m", Some(&a), submission());
        assert_eq!(again.debrief.len(), 2);
        assert!(again.debrief[0].is_answered());
    }

    #[test]
    fn a_skip_is_recorded_unless_the_user_also_wrote_an_answer() {
        let mut a = Analysis::from_submission("sess", "m", None, submission());
        a.set_open_questions(vec![
            question("A?", QuestionKind::Outcome),
            question("B?", QuestionKind::Gotcha),
        ]);
        a.answer_debrief(&[
            DebriefAnswer { id: "q1".into(), answer: None, skipped: true },
            DebriefAnswer { id: "q2".into(), answer: Some("both".into()), skipped: true },
        ]);
        assert!(a.debrief[0].skipped && !a.debrief[0].is_open());
        assert!(a.debrief[1].is_answered() && !a.debrief[1].skipped);
        assert_eq!(a.open_questions(), 0);
        // Answering again with the same content changes nothing.
        let before = a.revision;
        assert_eq!(a.answer_debrief(&[DebriefAnswer { id: "q2".into(), answer: Some(" both ".into()), skipped: false }]), 0);
        assert_eq!(a.revision, before);
    }

    #[test]
    fn only_answered_questions_reach_the_builder() {
        let mut a = Analysis::from_submission("sess", "m", None, submission());
        a.set_open_questions(vec![
            question("What if the search returns nothing?", QuestionKind::Exception),
            question("Unanswered?", QuestionKind::Variable),
        ]);
        assert!(!a.render().contains("## Debrief"), "no answers, no section");
        a.answer_debrief(&[DebriefAnswer { id: "q1".into(), answer: Some("Try the archive.".into()), skipped: false }]);
        let rendered = a.render();
        assert!(rendered.contains("## Debrief"));
        assert!(rendered.contains("(exception, about s1) What if the search returns nothing?"));
        assert!(rendered.contains("Answer: Try the archive."));
        assert!(!rendered.contains("Unanswered?"));
    }

    #[test]
    fn question_kinds_parse_the_ways_models_spell_them() {
        assert_eq!(QuestionKind::parse("Decision"), QuestionKind::Decision);
        assert_eq!(QuestionKind::parse("why"), QuestionKind::Decision);
        assert_eq!(QuestionKind::parse("variable"), QuestionKind::Variable);
        assert_eq!(QuestionKind::parse("inputs"), QuestionKind::Variable);
        assert_eq!(QuestionKind::parse("pre-condition"), QuestionKind::Precondition);
        assert_eq!(QuestionKind::parse("outcome"), QuestionKind::Outcome);
        assert_eq!(QuestionKind::parse("Done criteria"), QuestionKind::Outcome);
        assert_eq!(QuestionKind::parse("gotcha"), QuestionKind::Gotcha);
        assert_eq!(QuestionKind::parse("edge case"), QuestionKind::Exception);
        assert_eq!(QuestionKind::parse(""), QuestionKind::Exception);
        // The on-disk spelling is the label.
        assert_eq!(serde_json::to_string(&QuestionKind::Gotcha).unwrap(), "\"gotcha\"");
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
