//! The skill builder: approved analysis → a reviewable plan → `SKILL.md`.
//!
//! Two phases with the user in between, and the second phase is *constrained* by
//! what they approved. The model does not get to quietly re-plan while writing
//! the body: the approved plan is replayed into the prompt as the spec, and the
//! values the user edited are the ones that get substituted.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use skillrec_core::config::LlmConfig;
use skillrec_core::session::write_json;
use skillrec_core::skill::{
    render_skill_markdown, slugify, unresolved_tokens, BuiltSkill, FixedValue, SkillPlan,
    SkillSubmission,
};

use crate::agent::{parse_arguments, schema, Agent, AgentProgress, Tool, ToolOutput};
use crate::client::{LlmClient, ToolDef};
use crate::instructions::SKILL_BUILDER;
use crate::session_data::SessionData;

type Shared = Arc<SessionData>;

/// Also the debrief's first read: it asks about the analysis the user approved.
pub(crate) struct GetAnalysis(pub(crate) Shared);

#[async_trait::async_trait]
impl Tool for GetAnalysis {
    fn name(&self) -> &'static str {
        "get_analysis"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "get_analysis",
            "The approved intent and ordered steps you are generalizing. Read this first.",
            schema(json!({}), &[]),
        )
    }

    async fn call(&self, _: Value) -> Result<ToolOutput> {
        let analysis = self
            .0
            .analysis
            .as_ref()
            .context("this recording has not been analysed yet")?;
        Ok(ToolOutput::text(analysis.render()))
    }
}

struct GetTimeline(Shared);

#[async_trait::async_trait]
impl Tool for GetTimeline {
    fn name(&self) -> &'static str {
        "get_timeline"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "get_timeline",
            "The deterministic timeline behind the analysis — apps, URLs, hosts and copies. Use \
             it to ground your tool mapping in what actually happened.",
            schema(json!({}), &[]),
        )
    }

    async fn call(&self, _: Value) -> Result<ToolOutput> {
        Ok(ToolOutput::json(&self.0.timeline_view()))
    }
}

struct ProposePlan {
    captured: Arc<Mutex<Option<SkillPlan>>>,
}

#[async_trait::async_trait]
impl Tool for ProposePlan {
    fn name(&self) -> &'static str {
        "propose_plan"
    }

    fn is_terminal(&self) -> bool {
        true
    }

    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "propose_plan",
            "Your reviewable plan: how you will generalize the task, the fixed values it \
             hard-codes (referenced from step text as {{id}}), and the ordered steps with the \
             tool each uses. Call once, then stop.",
            schema(
                json!({
                    "name": { "type": "string", "description": "kebab-case slug for the skill" },
                    "title": { "type": "string" },
                    "description": { "type": "string", "description": "the trigger: what it does AND when to use it" },
                    "summary": { "type": "string" },
                    "generalization": { "type": "string", "description": "how the single run generalizes" },
                    "values": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string", "description": "snake_case key" },
                                "name": { "type": "string" },
                                "value": { "type": "string" }
                            },
                            "required": ["id", "name", "value"]
                        }
                    },
                    "steps": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": { "type": "string" },
                                "text": { "type": "string" },
                                "kind": { "type": "string", "enum": ["calculation", "action"] },
                                "tool": { "type": "string" }
                            },
                            "required": ["title", "text"]
                        }
                    },
                    "allowedTools": { "type": "array", "items": { "type": "string" } }
                }),
                &["name", "description", "steps"],
            ),
        )
    }

    async fn call(&self, arguments: Value) -> Result<ToolOutput> {
        let mut plan: SkillPlan =
            parse_arguments(arguments.clone()).context("propose_plan had the wrong shape")?;
        plan.name = slugify(&plan.name);
        anyhow::ensure!(!plan.steps.is_empty(), "a plan needs at least one step");
        anyhow::ensure!(
            !plan.description.trim().is_empty(),
            "the description is how an agent decides to use this skill; it cannot be empty"
        );
        *self.captured.lock().unwrap() = Some(plan);
        Ok(ToolOutput::Terminal(arguments))
    }
}

struct SubmitSkill {
    captured: Arc<Mutex<Option<SkillSubmission>>>,
}

#[async_trait::async_trait]
impl Tool for SubmitSkill {
    fn name(&self) -> &'static str {
        "submit_skill"
    }

    fn is_terminal(&self) -> bool {
        true
    }

    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "submit_skill",
            "The final SKILL.md. `body` is the instructions, referencing each fixed value by its \
             {{id}} token rather than the literal.",
            schema(
                json!({
                    "name": { "type": "string" },
                    "description": { "type": "string" },
                    "allowedTools": { "type": "array", "items": { "type": "string" } },
                    "body": { "type": "string", "description": "the SKILL.md instructions, in imperative voice" }
                }),
                &["name", "description", "body"],
            ),
        )
    }

    async fn call(&self, arguments: Value) -> Result<ToolOutput> {
        let submission: SkillSubmission =
            parse_arguments(arguments.clone()).context("submit_skill had the wrong shape")?;
        anyhow::ensure!(!submission.body.trim().is_empty(), "the skill body cannot be empty");
        *self.captured.lock().unwrap() = Some(submission);
        Ok(ToolOutput::Terminal(arguments))
    }
}

/// Where a finished skill is written.
#[derive(Debug, Clone)]
pub enum SkillTarget {
    /// Into the agent's skills folder, where it is auto-loaded.
    Install,
    /// Into a folder the user picked.
    Export(std::path::PathBuf),
}

/// Runs the two-phase skill build.
pub struct SkillBuilder {
    config: LlmConfig,
}

impl SkillBuilder {
    pub fn new(config: LlmConfig) -> Self {
        Self { config }
    }

    /// Phase one: propose a plan, or refine the current one from feedback.
    pub async fn plan(
        &self,
        data: SessionData,
        previous: Option<&SkillPlan>,
        feedback: Option<&str>,
        on_progress: &(dyn Fn(AgentProgress) + Send + Sync),
    ) -> Result<SkillPlan> {
        anyhow::ensure!(
            data.analysis.is_some(),
            "analyse this recording before building a skill from it"
        );
        let session_id = data.id.clone();
        let data = Arc::new(data);
        let captured: Arc<Mutex<Option<SkillPlan>>> = Arc::new(Mutex::new(None));

        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(GetAnalysis(Arc::clone(&data))),
            Box::new(GetTimeline(Arc::clone(&data))),
            Box::new(ProposePlan { captured: Arc::clone(&captured) }),
        ];

        let client = LlmClient::new(self.config.clone())?;
        let mut agent = Agent::new(client, &session_id, SKILL_BUILDER.trim().to_string(), tools);

        on_progress(AgentProgress {
            session_id: session_id.clone(),
            phase: "start".into(),
            message: if feedback.is_some() {
                "Refining the plan…".into()
            } else {
                "Planning the skill…".into()
            },
        });

        let prompt = match (feedback, previous) {
            (Some(feedback), Some(previous)) => format!(
                "The user reviewed your plan and asked for these changes:\n\n{feedback}\n\n\
                 Your previous plan was:\n\n{}\n\n\
                 Call propose_plan again with the revision.",
                render_plan(previous)
            ),
            _ => "Read get_analysis, and get_timeline where the tool mapping needs evidence. Then \
                  call propose_plan with how you will generalize this task, its fixed values, and \
                  its ordered steps. Stop after propose_plan so the user can review it."
                .to_string(),
        };

        agent.run_turn(prompt, on_progress).await?;
        captured
            .lock()
            .unwrap()
            .take()
            .context("the model finished without proposing a plan")
    }

    /// Phase two: build the SKILL.md from the plan the user approved.
    ///
    /// `plan` is the *edited* plan, so any value the user changed is what gets
    /// written — the model is explicitly told not to re-plan.
    pub async fn build(
        &self,
        data: SessionData,
        plan: &SkillPlan,
        target: SkillTarget,
        on_progress: &(dyn Fn(AgentProgress) + Send + Sync),
    ) -> Result<(BuiltSkill, std::path::PathBuf)> {
        let session_id = data.id.clone();
        let dir = data.dir.clone();
        let data = Arc::new(data);
        let captured: Arc<Mutex<Option<SkillSubmission>>> = Arc::new(Mutex::new(None));

        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(GetAnalysis(Arc::clone(&data))),
            Box::new(SubmitSkill { captured: Arc::clone(&captured) }),
        ];

        let client = LlmClient::new(self.config.clone())?;
        let mut agent = Agent::new(client, &session_id, SKILL_BUILDER.trim().to_string(), tools);

        on_progress(AgentProgress {
            session_id: session_id.clone(),
            phase: "start".into(),
            message: "Writing the skill…".into(),
        });

        let prompt = format!(
            "The user reviewed and edited this plan. Build the SKILL.md from EXACTLY this plan — \
             do not add, drop, reorder or rename its values or steps.\n\n{}\n\n\
             Call submit_skill with a generalized, tool-first instructions body that follows these \
             steps faithfully and references each fixed value by its {{{{id}}}} token, never the \
             literal. The name and description are already decided; echo them.",
            render_plan(plan)
        );

        agent.run_turn(prompt, on_progress).await?;
        let submission = captured
            .lock()
            .unwrap()
            .take()
            .context("the model finished without submitting a skill")?;

        // The user's edited values win over anything the model echoed back.
        let skill = BuiltSkill {
            session_id: session_id.clone(),
            name: slugify(&submission.name),
            description: submission.description.trim().to_string(),
            allowed_tools: if submission.allowed_tools.is_empty() {
                plan.allowed_tools.clone()
            } else {
                submission.allowed_tools
            },
            body: submission.body,
            values: plan.values.clone(),
            model: self.config.model.clone(),
        };

        let missing = unresolved_tokens(&skill.body, &skill.values);
        if !missing.is_empty() {
            // Not fatal — the token stays visible in the output — but the user
            // must be told, because a `{{typo}}` in a shell command is a bug
            // that only shows up when the skill runs.
            tracing::warn!(?missing, "the skill body references values the plan does not define");
            on_progress(AgentProgress {
                session_id: session_id.clone(),
                phase: "warning".into(),
                message: format!("Undefined values referenced: {}", missing.join(", ")),
            });
        }

        write_json(&dir.join("skill.json"), &skill).context("saving skill.json")?;
        let written = write_skill(&skill, target)?;

        on_progress(AgentProgress {
            session_id,
            phase: "done".into(),
            message: format!("Wrote {}", written.display()),
        });
        Ok((skill, written))
    }
}

/// Write `<root>/<name>/SKILL.md`.
fn write_skill(skill: &BuiltSkill, target: SkillTarget) -> Result<std::path::PathBuf> {
    let root = match target {
        SkillTarget::Install => skillrec_core::paths::skills_root()?,
        SkillTarget::Export(dir) => dir,
    };
    let folder = root.join(&skill.name);
    std::fs::create_dir_all(&folder)
        .with_context(|| format!("creating {}", folder.display()))?;
    let file = folder.join("SKILL.md");
    std::fs::write(&file, render_skill_markdown(skill))
        .with_context(|| format!("writing {}", file.display()))?;
    Ok(file)
}

/// Render a plan back into the prompt, so phase two builds what was approved.
pub fn render_plan(plan: &SkillPlan) -> String {
    let mut out = format!(
        "## Plan: {}\n\nname: {}\ndescription: {}\n",
        if plan.title.is_empty() { &plan.name } else { &plan.title },
        plan.name,
        plan.description
    );
    if !plan.generalization.is_empty() {
        out.push_str(&format!("\n**Generalization:** {}\n", plan.generalization));
    }
    if !plan.values.is_empty() {
        out.push_str("\n### Fixed values\n\n");
        for value in &plan.values {
            out.push_str(&format!("- `{{{{{}}}}}` ({}) = {}\n", value.id, value.name, value.value));
        }
    }
    out.push_str("\n### Steps\n\n");
    for (index, step) in plan.steps.iter().enumerate() {
        out.push_str(&format!(
            "{}. [{}] **{}** — {}{}\n",
            index + 1,
            match step.kind {
                skillrec_core::skill::StepKind::Action => "action",
                skillrec_core::skill::StepKind::Calculation => "calculation",
            },
            step.title,
            step.text,
            if step.tool.is_empty() { String::new() } else { format!(" _(via {})_", step.tool) }
        ));
    }
    if !plan.allowed_tools.is_empty() {
        out.push_str(&format!("\nallowed-tools: {}\n", plan.allowed_tools.join(", ")));
    }
    out
}

/// Apply user edits to a plan's values before the build phase.
pub fn apply_value_edits(plan: &mut SkillPlan, edits: &[FixedValue]) {
    for edit in edits {
        if let Some(existing) = plan.values.iter_mut().find(|v| v.id == edit.id) {
            existing.value = edit.value.clone();
            if !edit.name.trim().is_empty() {
                existing.name = edit.name.clone();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skillrec_core::skill::{PlanStep, StepKind};

    fn plan() -> SkillPlan {
        SkillPlan {
            name: "Sync Backlog!".into(),
            title: "Sync the backlog".into(),
            description: "Sync backlog issues. Use when asked to refresh the issue list.".into(),
            summary: String::new(),
            generalization: "Handles every row, not the three in the recording.".into(),
            values: vec![FixedValue {
                id: "repo".into(),
                name: "Repository".into(),
                value: "acme/widgets".into(),
            }],
            steps: vec![
                PlanStep {
                    title: "Read the backlog".into(),
                    text: "Fetch every open row.".into(),
                    kind: StepKind::Calculation,
                    tool: "web_fetch".into(),
                },
                PlanStep {
                    title: "File the issues".into(),
                    text: "Create one issue per row in {{repo}}.".into(),
                    kind: StepKind::Action,
                    tool: "Bash(gh *)".into(),
                },
            ],
            allowed_tools: vec!["Bash(gh *)".into()],
        }
    }

    #[tokio::test]
    async fn a_proposed_plan_is_slugified_and_captured() {
        let captured = Arc::new(Mutex::new(None));
        let tool = ProposePlan { captured: Arc::clone(&captured) };
        let output = tool
            .call(json!({
                "name": "Sync Backlog!",
                "description": "Sync issues. Use when refreshing the backlog.",
                "steps": [{"title": "Read", "text": "Fetch the rows."}]
            }))
            .await
            .unwrap();
        assert!(matches!(output, ToolOutput::Terminal(_)));
        assert_eq!(captured.lock().unwrap().as_ref().unwrap().name, "sync-backlog");
    }

    #[tokio::test]
    async fn a_plan_without_a_description_is_rejected() {
        // The description is the trigger; without it the skill never fires.
        let tool = ProposePlan { captured: Arc::new(Mutex::new(None)) };
        assert!(tool
            .call(json!({"name": "x", "description": "  ", "steps": [{"title": "a", "text": "b"}]}))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_plan_with_no_steps_is_rejected() {
        let tool = ProposePlan { captured: Arc::new(Mutex::new(None)) };
        assert!(tool
            .call(json!({"name": "x", "description": "does a thing when asked", "steps": []}))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn an_empty_skill_body_is_rejected() {
        let tool = SubmitSkill { captured: Arc::new(Mutex::new(None)) };
        assert!(tool
            .call(json!({"name": "x", "description": "d", "body": "   "}))
            .await
            .is_err());
    }

    #[test]
    fn a_rendered_plan_carries_everything_phase_two_needs() {
        let rendered = render_plan(&plan());
        assert!(rendered.contains("{{repo}}"));
        assert!(rendered.contains("acme/widgets"));
        assert!(rendered.contains("[action] **File the issues**"));
        assert!(rendered.contains("[calculation] **Read the backlog**"));
        assert!(rendered.contains("via Bash(gh *)"));
        assert!(rendered.contains("allowed-tools: Bash(gh *)"));
    }

    #[test]
    fn user_edits_to_values_survive_into_the_build() {
        // The user retargeting the skill at their own repo is the whole point of
        // the review step; the model must not be able to override it.
        let mut plan = plan();
        apply_value_edits(
            &mut plan,
            &[FixedValue { id: "repo".into(), name: String::new(), value: "mine/thing".into() }],
        );
        assert_eq!(plan.values[0].value, "mine/thing");
        assert_eq!(plan.values[0].name, "Repository", "a blank name must not clear the label");
    }

    #[test]
    fn edits_to_unknown_values_are_ignored() {
        let mut plan = plan();
        apply_value_edits(
            &mut plan,
            &[FixedValue { id: "nope".into(), name: "x".into(), value: "y".into() }],
        );
        assert_eq!(plan.values.len(), 1);
        assert_eq!(plan.values[0].value, "acme/widgets");
    }

    #[test]
    fn a_built_skill_writes_a_skill_md_with_resolved_values() {
        let dir = std::env::temp_dir().join(format!("skillrec-build-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let skill = BuiltSkill {
            session_id: "s".into(),
            name: "sync-backlog".into(),
            description: "Sync issues.".into(),
            allowed_tools: vec!["Bash(gh *)".into()],
            body: "Create one issue per row in {{repo}}.".into(),
            values: plan().values,
            model: "test".into(),
        };
        let written = write_skill(&skill, SkillTarget::Export(dir.clone())).unwrap();
        let markdown = std::fs::read_to_string(&written).unwrap();
        assert!(written.ends_with("sync-backlog/SKILL.md"));
        assert!(markdown.contains("name: sync-backlog"));
        assert!(markdown.contains("acme/widgets"));
        assert!(!markdown.contains("{{repo}}"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
