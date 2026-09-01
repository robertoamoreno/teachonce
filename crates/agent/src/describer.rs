//! The describer: recording → intent + ordered steps.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use skillrec_core::analysis::{Analysis, AnalysisSubmission};
use skillrec_core::config::LlmConfig;
use skillrec_core::session::write_json;

use crate::agent::{arg_i64, arg_str, parse_arguments, schema, Agent, AgentProgress, Tool, ToolOutput};
use crate::client::{ContentPart, LlmClient, ToolDef};
use crate::instructions::DESCRIBER;
use crate::session_data::SessionData;

/// Cap on images returned by one `get_frames` call.
///
/// Each image costs roughly a thousand tokens and, on a local model, seconds of
/// latency. Six is enough to see a short interaction unfold; more than that and
/// the model is browsing the recording rather than resolving an ambiguity.
const MAX_FRAMES_PER_CALL: usize = 6;

/// Cap on events returned by one `get_events` call.
const MAX_EVENTS_PER_CALL: usize = 120;

type Shared = Arc<SessionData>;

struct GetTimeline(Shared);

#[async_trait::async_trait]
impl Tool for GetTimeline {
    fn name(&self) -> &'static str {
        "get_timeline"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "get_timeline",
            "The segmented timeline of the recording: ordered steps with their app, URLs, \
             window titles, copied text, spoken narration and atMs span. Always start here.",
            schema(json!({}), &[]),
        )
    }

    async fn call(&self, _: Value) -> Result<ToolOutput> {
        Ok(ToolOutput::json(&self.0.timeline_view()))
    }
}

struct GetNarration(Shared);

#[async_trait::async_trait]
impl Tool for GetNarration {
    fn name(&self) -> &'static str {
        "get_narration"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "get_narration",
            "The user's spoken narration as timestamped lines, in their own words. Optionally \
             pass `query` to search it. An empty result means the user did not narrate.",
            schema(json!({ "query": { "type": "string" } }), &[]),
        )
    }

    async fn call(&self, arguments: Value) -> Result<ToolOutput> {
        let Some(narration) = self.0.narration.as_ref().filter(|n| !n.is_empty()) else {
            return Ok(ToolOutput::text("The user did not record any narration."));
        };
        match arg_str(&arguments, &["query", "q"]).filter(|q| !q.trim().is_empty()) {
            Some(query) => {
                let hits = narration.grep(&query);
                if hits.is_empty() {
                    return Ok(ToolOutput::text(format!("No narration matches {query:?}.")));
                }
                Ok(ToolOutput::text(
                    hits.iter()
                        .map(|s| format!("[{}ms] {}", s.at_ms, s.text))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ))
            }
            None => Ok(ToolOutput::text(narration.render())),
        }
    }
}

struct GetEvents(Shared);

#[async_trait::async_trait]
impl Tool for GetEvents {
    fn name(&self) -> &'static str {
        "get_events"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "get_events",
            "The raw event stream with full titles, full URLs and clipboard text. Narrow it with \
             `fromMs`/`toMs` and `types` (app.activate, app.title-change, browser.url, \
             clipboard.change, marker).",
            schema(
                json!({
                    "fromMs": { "type": "integer", "description": "start of the window, in ms since the recording started" },
                    "toMs": { "type": "integer", "description": "end of the window" },
                    "types": { "type": "array", "items": { "type": "string" } }
                }),
                &[],
            ),
        )
    }

    async fn call(&self, arguments: Value) -> Result<ToolOutput> {
        let types: Vec<String> = arguments["types"]
            .as_array()
            .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        Ok(ToolOutput::json(&self.0.events_view(
            &types,
            arg_i64(&arguments, &["fromMs", "from_ms", "from"]),
            arg_i64(&arguments, &["toMs", "to_ms", "to"]),
            MAX_EVENTS_PER_CALL,
        )))
    }
}

struct ListFrames(Shared);

#[async_trait::async_trait]
impl Tool for ListFrames {
    fn name(&self) -> &'static str {
        "list_frames"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "list_frames",
            "Index of the screen stills available: file, atMs, and why each was kept \
             (changed / heartbeat / initial). Empty means no frames were captured.",
            schema(json!({}), &[]),
        )
    }

    async fn call(&self, _: Value) -> Result<ToolOutput> {
        if self.0.frames.frames.is_empty() {
            return Ok(ToolOutput::text("No screen frames were captured for this recording."));
        }
        let index: Vec<Value> = self
            .0
            .frames
            .frames
            .iter()
            .map(|frame| json!({ "file": frame.file, "atMs": frame.at_ms, "reason": frame.reason }))
            .collect();
        let mut reply = json!({ "frames": index });
        if self.0.frames.truncated {
            // Without this the model reads "no frames after 60 minutes" as "the
            // screen never changed after 60 minutes", which is the opposite of
            // what happened.
            reply["note"] = json!(format!(
                "Screen capture hit its per-recording limit at {}ms. There are NO frames after \
                 that point — this does not mean the screen stopped changing.",
                self.0.frames.covers_to_ms().unwrap_or(0)
            ));
        }
        Ok(ToolOutput::json(&reply))
    }
}

struct GetFrames(Shared);

#[async_trait::async_trait]
impl Tool for GetFrames {
    fn name(&self) -> &'static str {
        "get_frames"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "get_frames",
            "View the screen stills in a time window. Returns the images so you can actually \
             see the screen. Use this ONLY where the events leave real ambiguity.",
            schema(
                json!({
                    "fromMs": { "type": "integer", "description": "start of the window, in ms since the recording started" },
                    "toMs": { "type": "integer", "description": "end of the window" },
                    "max": { "type": "integer", "description": "at most 6" }
                }),
                &["fromMs", "toMs"],
            ),
        )
    }

    async fn call(&self, arguments: Value) -> Result<ToolOutput> {
        let from = arg_i64(&arguments, &["fromMs", "from_ms", "from"]).unwrap_or(0);
        let to = arg_i64(&arguments, &["toMs", "to_ms", "to"]).unwrap_or(i64::MAX);
        let max = arg_i64(&arguments, &["max", "limit"])
            .unwrap_or(MAX_FRAMES_PER_CALL as i64)
            .clamp(1, MAX_FRAMES_PER_CALL as i64) as usize;

        let picked = self.0.frames.window(from, to, max);
        if picked.is_empty() {
            // Point at the nearest one rather than just saying no: the model
            // asked about a moment it cares about, and an empty answer usually
            // means its window was slightly off, not that there is nothing.
            let hint = self
                .0
                .frames
                .nearest(from)
                .map(|f| format!(" The nearest frame is at {}ms.", f.at_ms))
                .unwrap_or_default();
            return Ok(ToolOutput::text(format!(
                "No frames were captured between {from}ms and {to}ms.{hint}"
            )));
        }

        let mut images = Vec::new();
        let mut labels = Vec::new();
        for frame in picked {
            match self.0.read_frame(&frame.file) {
                Ok(bytes) => {
                    labels.push(format!("{} at {}ms", frame.file, frame.at_ms));
                    images.push(ContentPart::jpeg(&bytes));
                }
                Err(err) => tracing::warn!(file = %frame.file, "could not read frame: {err:#}"),
            }
        }
        if images.is_empty() {
            anyhow::bail!("the frames in that window could not be read from disk");
        }
        Ok(ToolOutput::Images { text: format!("Frames: {}", labels.join(", ")), images })
    }
}

struct SubmitAnalysis {
    captured: Arc<Mutex<Option<AnalysisSubmission>>>,
}

#[async_trait::async_trait]
impl Tool for SubmitAnalysis {
    fn name(&self) -> &'static str {
        "submit_analysis"
    }

    fn is_terminal(&self) -> bool {
        true
    }

    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "submit_analysis",
            "Your required final action: the overall intent and the ordered steps the user took.",
            schema(
                json!({
                    "title": { "type": "string", "description": "2-5 words, Title Case, under 40 characters" },
                    "intent": { "type": "string", "description": "one sentence naming the user's goal" },
                    "intentConfidence": { "type": "string", "enum": ["high", "medium", "low"] },
                    "intentRationale": { "type": "string" },
                    "steps": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "title": { "type": "string", "description": "past tense, addressed to the user" },
                                "detail": { "type": "string" },
                                "startMs": { "type": "integer" },
                                "endMs": { "type": "integer" },
                                "apps": { "type": "array", "items": { "type": "string" } },
                                "evidence": { "type": "array", "items": { "type": "string" } },
                                "confidence": { "type": "string", "enum": ["high", "medium", "low"] }
                            },
                            "required": ["title"]
                        }
                    }
                }),
                &["title", "intent", "steps"],
            ),
        )
    }

    async fn call(&self, arguments: Value) -> Result<ToolOutput> {
        let submission: AnalysisSubmission = parse_arguments(arguments.clone())
            .context("submit_analysis was called with the wrong shape")?;
        anyhow::ensure!(!submission.intent.trim().is_empty(), "the intent cannot be empty");
        *self.captured.lock().unwrap() = Some(submission);
        Ok(ToolOutput::Terminal(arguments))
    }
}

/// Runs the describer for one recording.
pub struct Describer {
    config: LlmConfig,
}

impl Describer {
    pub fn new(config: LlmConfig) -> Self {
        Self { config }
    }

    /// Analyse a recording from scratch.
    pub async fn analyze(
        &self,
        data: SessionData,
        on_progress: &(dyn Fn(AgentProgress) + Send + Sync),
    ) -> Result<Analysis> {
        self.run(data, None, None, on_progress).await
    }

    /// Re-analyse with the user's feedback folded in.
    pub async fn revise(
        &self,
        data: SessionData,
        feedback: &str,
        on_progress: &(dyn Fn(AgentProgress) + Send + Sync),
    ) -> Result<Analysis> {
        let previous = data.analysis.clone();
        self.run(data, previous, Some(feedback), on_progress).await
    }

    async fn run(
        &self,
        data: SessionData,
        previous: Option<Analysis>,
        feedback: Option<&str>,
        on_progress: &(dyn Fn(AgentProgress) + Send + Sync),
    ) -> Result<Analysis> {
        let session_id = data.id.clone();
        let dir = data.dir.clone();
        let vision = self.config.vision;
        let data = Arc::new(data);
        let captured: Arc<Mutex<Option<AnalysisSubmission>>> = Arc::new(Mutex::new(None));

        let mut tools: Vec<Box<dyn Tool>> = vec![
            Box::new(GetTimeline(Arc::clone(&data))),
            Box::new(GetNarration(Arc::clone(&data))),
            Box::new(GetEvents(Arc::clone(&data))),
        ];
        // Offering frame tools to a text-only model wastes turns on calls the
        // server will reject, so they are simply absent unless vision is on.
        if vision && !data.frames.frames.is_empty() {
            tools.push(Box::new(ListFrames(Arc::clone(&data))));
            tools.push(Box::new(GetFrames(Arc::clone(&data))));
        }
        tools.push(Box::new(SubmitAnalysis { captured: Arc::clone(&captured) }));

        let client = LlmClient::new(self.config.clone())?;
        let model = self.config.model.clone();
        let mut agent = Agent::new(client, &session_id, DESCRIBER.trim().to_string(), tools);

        on_progress(AgentProgress {
            session_id: session_id.clone(),
            phase: "start".into(),
            message: if feedback.is_some() {
                "Re-analysing with your feedback…".into()
            } else {
                "Analysing the recording…".into()
            },
        });

        let prompt = match (feedback, previous.as_ref()) {
            (Some(feedback), Some(previous)) => format!(
                "The user reviewed your analysis and gave this feedback:\n\n{feedback}\n\n\
                 Your previous analysis was:\n\n{}\n\n\
                 Treat the feedback as authoritative. Re-examine the relevant signals, then call \
                 submit_analysis with a fully revised analysis. Keep step ids stable where a step \
                 is unchanged.",
                previous.render()
            ),
            (Some(feedback), None) => format!(
                "Reconstruct what the user did in this recording. Keep this in mind: {feedback}\n\n\
                 Start with get_timeline, then submit_analysis."
            ),
            _ => "Reconstruct what the user did in this recording. Start with get_timeline, read \
                  the narration if there is any, then read events where anything is unclear. Look \
                  at frames only where the events are ambiguous. When confident, call \
                  submit_analysis."
                .to_string(),
        };

        agent.run_turn(prompt, on_progress).await?;

        let submission = captured
            .lock()
            .unwrap()
            .take()
            .context("the model finished without submitting an analysis")?;

        let mut analysis =
            Analysis::from_submission(&session_id, &model, previous.as_ref(), submission);
        if let Some(feedback) = feedback {
            analysis.log_feedback(feedback);
        }

        write_json(&dir.join("analysis.json"), &analysis)
            .context("saving analysis.json")?;
        on_progress(AgentProgress {
            session_id,
            phase: "done".into(),
            message: format!("{} steps reconstructed.", analysis.steps.len()),
        });
        Ok(analysis)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_narration_tool_is_explicit_about_absence() {
        // "The user did not narrate" is actionable; an empty string reads to a
        // model like a failed call it should retry.
        let data = Arc::new(fixture(None));
        let output = GetNarration(data).call(json!({})).await.unwrap();
        match output {
            ToolOutput::Text(text) => assert!(text.contains("did not record any narration")),
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn narration_can_be_searched() {
        let narration = skillrec_core::narration::NarrationTranscript {
            model: "ggml-small".into(),
            language: "en".into(),
            segments: vec![
                skillrec_core::narration::NarrationSegment {
                    at_ms: 1_000,
                    end_ms: 2_000,
                    text: "opening the pricing page".into(),
                },
                skillrec_core::narration::NarrationSegment {
                    at_ms: 5_000,
                    end_ms: 6_000,
                    text: "now the invoice".into(),
                },
            ],
        };
        let tool = GetNarration(Arc::new(fixture(Some(narration))));

        let hit = tool.call(json!({"query": "PRICING"})).await.unwrap();
        match hit {
            ToolOutput::Text(text) => {
                assert!(text.contains("pricing"));
                assert!(!text.contains("invoice"));
            }
            _ => panic!("expected text"),
        }

        let miss = tool.call(json!({"query": "nothing"})).await.unwrap();
        match miss {
            ToolOutput::Text(text) => assert!(text.contains("No narration matches")),
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn a_truncated_frame_index_says_so_explicitly() {
        let mut data = fixture(None);
        data.frames = skillrec_core::frames::FrameManifest::with_truncation(
            vec![skillrec_core::frames::FrameRecord {
                file: "frames/frame_000001.jpg".into(),
                at_ms: 3_600_000,
                epoch: 3_601_000,
                reason: skillrec_core::events::FrameReason::Changed,
                phash: "0".repeat(16),
                width: 1280,
                height: 720,
                bytes: 10,
            }],
            true,
        );
        let output = ListFrames(Arc::new(data)).call(json!({})).await.unwrap();
        match output {
            ToolOutput::Text(text) => {
                assert!(text.contains("hit its per-recording limit"), "{text}");
                assert!(text.contains("3600000"));
            }
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn asking_for_frames_that_do_not_exist_points_at_the_nearest_one() {
        let mut data = fixture(None);
        data.frames = skillrec_core::frames::FrameManifest::new(vec![
            skillrec_core::frames::FrameRecord {
                file: "frames/frame_000001.jpg".into(),
                at_ms: 30_000,
                epoch: 31_000,
                reason: skillrec_core::events::FrameReason::Changed,
                phash: "0".repeat(16),
                width: 1280,
                height: 720,
                bytes: 10,
            },
        ]);
        let output = GetFrames(Arc::new(data)).call(json!({"fromMs": 0, "toMs": 1_000})).await.unwrap();
        match output {
            ToolOutput::Text(text) => {
                assert!(text.contains("No frames"));
                assert!(text.contains("30000ms"), "must hint at the nearest frame: {text}");
            }
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn submitting_without_an_intent_is_rejected() {
        let tool = SubmitAnalysis { captured: Arc::new(Mutex::new(None)) };
        let result = tool
            .call(json!({"title": "Something", "intent": "   ", "steps": []}))
            .await;
        assert!(result.is_err(), "an empty intent must not be accepted");
    }

    #[tokio::test]
    async fn a_valid_submission_ends_the_turn_and_is_captured() {
        let captured = Arc::new(Mutex::new(None));
        let tool = SubmitAnalysis { captured: Arc::clone(&captured) };
        let output = tool
            .call(json!({
                "title": "Check Pricing",
                "intent": "Compare the pricing tiers",
                "steps": [{"title": "Opened the pricing page"}]
            }))
            .await
            .unwrap();
        assert!(matches!(output, ToolOutput::Terminal(_)));
        let submission = captured.lock().unwrap().take().unwrap();
        assert_eq!(submission.title, "Check Pricing");
        assert_eq!(submission.steps.len(), 1);
    }

    #[test]
    fn frame_requests_are_capped_however_many_are_asked_for() {
        let arguments = json!({"fromMs": 0, "toMs": 10_000, "max": 500});
        let max = arg_i64(&arguments, &["max"])
            .unwrap_or(MAX_FRAMES_PER_CALL as i64)
            .clamp(1, MAX_FRAMES_PER_CALL as i64);
        assert_eq!(max, MAX_FRAMES_PER_CALL as i64);
    }

    fn fixture(narration: Option<skillrec_core::narration::NarrationTranscript>) -> SessionData {
        let meta = skillrec_core::session::SessionMeta {
            id: "fixture".into(),
            started_at: 1_000,
            stopped_at: Some(61_000),
            platform: "macos".into(),
            app_version: "test".into(),
            narrated: narration.is_some(),
            title: None,
        };
        let bundle = skillrec_core::timeline::build_bundle(&meta, &[]);
        SessionData {
            id: meta.id.clone(),
            dir: std::env::temp_dir(),
            meta,
            events: Vec::new(),
            bundle,
            narration,
            frames: Default::default(),
            analysis: None,
        }
    }
}
