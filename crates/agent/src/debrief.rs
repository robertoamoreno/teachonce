//! The debrief: after an analysis, up to five questions the recording alone
//! cannot answer.
//!
//! A demonstration shows one run on the happy path. It cannot show why a choice
//! was made, what varies from run to run, what the user does when something is
//! off, or how they know the task is done. Those are exactly the things a skill
//! needs and a recording lacks, so once the describer has reconstructed the
//! steps, a second agent reads the same evidence and asks. The user's answers
//! are stored on the analysis and reach the builder as facts about the task.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use skillrec_core::analysis::{Analysis, DebriefQuestion, QuestionKind, MAX_DEBRIEF_QUESTIONS};
use skillrec_core::config::LlmConfig;

use crate::agent::{parse_arguments, schema, Agent, AgentProgress, Tool, ToolOutput};
use crate::builder::GetAnalysis;
use crate::client::{LlmClient, ToolDef};
use crate::describer::{GetEvents, GetNarration, GetTimeline};
use crate::instructions::DEBRIEF;
use crate::session_data::SessionData;

/// One question as the model sends it. Loose on `kind`, so a misfiled
/// question is still asked rather than dropped.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawQuestion {
    question: String,
    #[serde(default)]
    why: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default, alias = "step")]
    step_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct QuestionSubmission {
    #[serde(default)]
    questions: Vec<RawQuestion>,
}

struct SubmitQuestions {
    captured: Arc<Mutex<Option<Vec<DebriefQuestion>>>>,
}

#[async_trait::async_trait]
impl Tool for SubmitQuestions {
    fn name(&self) -> &'static str {
        "submit_questions"
    }

    fn is_terminal(&self) -> bool {
        true
    }

    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "submit_questions",
            "Your required final action: the questions the recording cannot answer on its own, \
             at most five.",
            schema(
                json!({
                    "questions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": { "type": "string", "description": "one thing, addressed to the user, answerable in a sentence or two" },
                                "why": { "type": "string", "description": "what in the recording prompted it, citing the step id" },
                                "kind": { "type": "string", "enum": ["exception", "decision", "variable", "precondition", "outcome", "gotcha"] },
                                "stepId": { "type": "string", "description": "the analysis step it is about, e.g. s2" }
                            },
                            "required": ["question", "why", "kind"]
                        }
                    }
                }),
                &["questions"],
            ),
        )
    }

    async fn call(&self, arguments: Value) -> Result<ToolOutput> {
        let submission: QuestionSubmission = parse_arguments(arguments.clone())
            .context("submit_questions was called with the wrong shape")?;
        let questions: Vec<DebriefQuestion> = submission
            .questions
            .into_iter()
            .filter_map(|raw| {
                let text = raw.question.trim();
                if text.is_empty() {
                    return None;
                }
                // Seen live: the model cites the step in `why` ("s2 shows…")
                // far more reliably than it fills `stepId`.
                let step_id = raw
                    .step_id
                    .filter(|s| !s.trim().is_empty())
                    .or_else(|| step_cited_in(&raw.why));
                Some(DebriefQuestion {
                    id: String::new(),
                    question: text.to_string(),
                    why: raw.why.trim().to_string(),
                    kind: raw.kind.as_deref().map(QuestionKind::parse).unwrap_or_default(),
                    step_id,
                    answer: None,
                    skipped: false,
                })
            })
            .collect();
        anyhow::ensure!(
            !questions.is_empty(),
            "ask at least one question, or the debrief has no point"
        );
        if questions.len() > MAX_DEBRIEF_QUESTIONS {
            tracing::info!(
                asked = questions.len(),
                "the model asked more than {MAX_DEBRIEF_QUESTIONS} questions; keeping the first"
            );
        }
        *self.captured.lock().unwrap() =
            Some(questions.into_iter().take(MAX_DEBRIEF_QUESTIONS).collect());
        Ok(ToolOutput::Terminal(arguments))
    }
}

/// The first step id (`s1`, `s12`) mentioned as a word in `text`, if any.
fn step_cited_in(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let boundary = |i: usize| i >= bytes.len() || !bytes[i].is_ascii_alphanumeric();
    let mut start = 0;
    while start < bytes.len() {
        if bytes[start] == b's' && (start == 0 || boundary(start - 1)) {
            let mut end = start + 1;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end > start + 1 && boundary(end) {
                return Some(text[start..end].to_string());
            }
        }
        start += 1;
    }
    None
}

/// Runs the debrief for one analysed recording.
pub struct Debriefer {
    config: LlmConfig,
}

impl Debriefer {
    pub fn new(config: LlmConfig) -> Self {
        Self { config }
    }

    /// Ask up to five questions about an analysed recording.
    ///
    /// The questions are returned, not stored: the caller folds them into the
    /// analysis with [`Analysis::set_open_questions`], which keeps whatever the
    /// user already answered.
    pub async fn ask(
        &self,
        data: SessionData,
        on_progress: &(dyn Fn(AgentProgress) + Send + Sync),
    ) -> Result<Vec<DebriefQuestion>> {
        let analysis = data
            .analysis
            .clone()
            .context("analyse this recording before debriefing it")?;
        let session_id = data.id.clone();
        let data = Arc::new(data);
        let captured: Arc<Mutex<Option<Vec<DebriefQuestion>>>> = Arc::new(Mutex::new(None));

        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(GetAnalysis(Arc::clone(&data))),
            Box::new(GetTimeline(Arc::clone(&data))),
            Box::new(GetNarration(Arc::clone(&data))),
            Box::new(GetEvents(Arc::clone(&data))),
            Box::new(SubmitQuestions { captured: Arc::clone(&captured) }),
        ];

        let client = LlmClient::new(self.config.clone())?;
        let mut agent = Agent::new(client, &session_id, DEBRIEF.trim().to_string(), tools);

        on_progress(AgentProgress {
            session_id: session_id.clone(),
            phase: "start".into(),
            message: "Thinking of questions the recording can't answer…".into(),
        });

        agent.run_turn(debrief_prompt(&analysis), on_progress).await?;

        let questions = captured
            .lock()
            .unwrap()
            .take()
            .context("the model finished without asking anything")?;

        on_progress(AgentProgress {
            session_id,
            phase: "done".into(),
            message: format!(
                "{} question{} to answer.",
                questions.len(),
                if questions.len() == 1 { "" } else { "s" }
            ),
        });
        Ok(questions)
    }
}

/// The user turn that opens a debrief.
///
/// The rendered analysis already carries every answered question, so the
/// model sees what is settled. Skipped questions are listed separately: they
/// are not in the render, and asking them again is the fastest way to make
/// someone stop answering.
fn debrief_prompt(analysis: &Analysis) -> String {
    let mut prompt = format!(
        "Here is the analysis of the recording:\n\n{}\n\n",
        analysis.render()
    );
    let skipped: Vec<&DebriefQuestion> = analysis.debrief.iter().filter(|q| q.skipped).collect();
    if analysis.debrief.iter().any(|q| q.is_answered()) || !skipped.is_empty() {
        prompt.push_str(
            "Some questions were already asked. Those with answers appear in the Debrief \
             section above; do not ask them again, nor anything their answers settle.",
        );
        if !skipped.is_empty() {
            prompt.push_str(" These were asked and the user chose to skip them, so do not ask \
                              them again either:\n");
            for question in skipped {
                prompt.push_str(&format!("- {}\n", question.question));
            }
        }
        prompt.push_str("\n\n");
    }
    prompt.push_str(
        "Read get_timeline, then get_narration if the user narrated, and get_events where a step \
         is unclear. Then call submit_questions with at most five questions the recording cannot \
         answer on its own. Prefer the ones whose answers would change what an agent should do.",
    );
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use skillrec_core::analysis::{AnalysisSubmission, Confidence, DebriefAnswer};

    fn analysis() -> Analysis {
        Analysis::from_submission(
            "sess",
            "m",
            None,
            AnalysisSubmission {
                title: "Check Pricing".into(),
                intent: "Compare the pricing tiers".into(),
                intent_confidence: Confidence::High,
                intent_rationale: String::new(),
                steps: Vec::new(),
            },
        )
    }

    #[tokio::test]
    async fn a_submission_is_captured_capped_and_kinds_parsed_loosely() {
        let captured = Arc::new(Mutex::new(None));
        let tool = SubmitQuestions { captured: Arc::clone(&captured) };
        let mut questions: Vec<Value> = (1..=6)
            .map(|i| json!({ "question": format!("Q{i}?"), "why": "s1", "kind": "edge case" }))
            .collect();
        questions.insert(1, json!({ "question": "   ", "why": "blank, dropped", "kind": "decision" }));
        questions.insert(
            2,
            json!({ "question": "Why the guide first?", "why": "s2", "kind": "Why", "step": "s2" }),
        );

        let output = tool.call(json!({ "questions": questions })).await.unwrap();
        assert!(matches!(output, ToolOutput::Terminal(_)));

        let kept = captured.lock().unwrap().take().unwrap();
        assert_eq!(kept.len(), MAX_DEBRIEF_QUESTIONS, "capped at five");
        assert_eq!(kept[0].question, "Q1?");
        assert_eq!(kept[0].kind, QuestionKind::Exception, "unknown spellings land on exception");
        assert_eq!(kept[0].step_id.as_deref(), Some("s1"), "a step cited in `why` fills stepId");
        assert_eq!(kept[1].question, "Why the guide first?");
        assert_eq!(kept[1].kind, QuestionKind::Decision);
        assert_eq!(kept[1].step_id.as_deref(), Some("s2"), "`step` is accepted for stepId");
        assert!(kept.iter().all(|q| q.answer.is_none() && !q.skipped));
    }

    #[tokio::test]
    async fn an_empty_or_blank_submission_is_rejected() {
        let tool = SubmitQuestions { captured: Arc::new(Mutex::new(None)) };
        assert!(tool.call(json!({ "questions": [] })).await.is_err());
        assert!(tool
            .call(json!({ "questions": [{ "question": " ", "why": "", "kind": "outcome" }] }))
            .await
            .is_err());
        assert!(tool.call(json!({})).await.is_err());
    }

    #[test]
    fn step_ids_are_recovered_from_prose_but_not_from_lookalikes() {
        assert_eq!(step_cited_in("s2 shows selecting a case number").as_deref(), Some("s2"));
        assert_eq!(step_cited_in("In step s12, the user copied it.").as_deref(), Some("s12"));
        assert_eq!(step_cited_in("The user's browser was Safari").as_deref(), None);
        assert_eq!(step_cited_in("windows10 s3d").as_deref(), None);
        assert_eq!(step_cited_in("").as_deref(), None);
    }

    #[test]
    fn the_prompt_carries_answers_and_names_skipped_questions() {
        let fresh = debrief_prompt(&analysis());
        assert!(fresh.contains("# Check Pricing"));
        assert!(!fresh.contains("already asked"));
        assert!(fresh.contains("submit_questions"));

        let mut answered = analysis();
        answered.set_open_questions(vec![
            DebriefQuestion {
                id: String::new(),
                question: "What if the page is empty?".into(),
                why: "s1".into(),
                kind: QuestionKind::Exception,
                step_id: None,
                answer: None,
                skipped: false,
            },
            DebriefQuestion {
                id: String::new(),
                question: "Which account do you use?".into(),
                why: "s1".into(),
                kind: QuestionKind::Precondition,
                step_id: None,
                answer: None,
                skipped: false,
            },
        ]);
        answered.answer_debrief(&[
            DebriefAnswer { id: "q1".into(), answer: Some("I stop.".into()), skipped: false },
            DebriefAnswer { id: "q2".into(), answer: None, skipped: true },
        ]);
        let prompt = debrief_prompt(&answered);
        assert!(prompt.contains("Answer: I stop."), "answered questions ride in the render");
        assert!(prompt.contains("chose to skip"));
        assert!(prompt.contains("- Which account do you use?"));
    }
}
