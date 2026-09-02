//! `POST /api/rpc/{command}`: the desktop app's command surface over HTTP.
//!
//! One route, one command name, one JSON body of arguments, exactly like a
//! Tauri `invoke`. That is what lets the browser run the same UI code: its
//! transport swaps `invoke` for this route and nothing above it changes.
//! Commands that only make sense with a screen to record are refused by name.

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, to_value, Value};
use skillrec_agent::{Debriefer, Describer, LlmClient, SessionData, SkillBuilder, SkillTarget};
use skillrec_core::analysis::{Analysis, AnalysisStep, DebriefAnswer};
use skillrec_core::config::Settings;
use skillrec_core::session::{read_json, set_session_title, write_json};
use skillrec_core::skill::{BuiltSkill, FixedValue};

use crate::config::new_api_key;
use crate::jobs;
use crate::state::{AppState, JobStatus};

pub async fn handle(
    State(state): State<Arc<AppState>>,
    Path(command): Path<String>,
    body: Bytes,
) -> Response {
    let args = if body.is_empty() {
        Value::Object(Default::default())
    } else {
        match serde_json::from_slice::<Value>(&body) {
            Ok(value) => value,
            Err(err) => {
                return (StatusCode::BAD_REQUEST, format!("the arguments are not JSON: {err}"))
                    .into_response()
            }
        }
    };
    match dispatch(&state, &command, args).await {
        Ok(value) => Json(value).into_response(),
        Err(err) => (StatusCode::BAD_REQUEST, format!("{err:#}")).into_response(),
    }
}

fn arg<T: DeserializeOwned>(args: &Value) -> Result<T> {
    serde_json::from_value(args.clone()).context("the arguments did not match the command")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WithId {
    id: String,
}

/// Everything one recording holds, for the detail view. The same shape the
/// desktop app returns, plus the server-side job status.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionDetail {
    summary: skillrec_core::session::SessionSummary,
    description: String,
    timeline: Value,
    narration: Option<skillrec_core::narration::NarrationTranscript>,
    analysis: Option<Analysis>,
    skill: Option<BuiltSkill>,
    frames: Vec<skillrec_core::frames::FrameRecord>,
    needs_transcription: bool,
    transcribe_via: skillrec_core::config::TranscriptionBackend,
    transcribe_host: String,
    server_url: Option<String>,
    job: Option<JobStatus>,
}

async fn detail(state: &Arc<AppState>, id: &str) -> Result<SessionDetail> {
    let dir = skillrec_core::paths::session_dir(id)?;
    let data = SessionData::load(&dir)?;
    let summary = skillrec_core::session::list_sessions()?
        .into_iter()
        .find(|s| s.meta.id == id)
        .with_context(|| format!("no recording called {id}"))?;
    let narration = state.config.read().await.settings.narration.clone();
    Ok(SessionDetail {
        description: std::fs::read_to_string(dir.join("description.md")).unwrap_or_default(),
        timeline: to_value(data.timeline_view()).unwrap_or_default(),
        narration: data.narration.clone(),
        analysis: data.analysis.clone(),
        skill: read_json(&dir.join("skill.json")),
        frames: data.frames.frames.clone(),
        needs_transcription: skillrec_narration::transcribe::needs_transcription(&dir),
        transcribe_via: narration.backend,
        transcribe_host: skillrec_core::timeline::host_of(&narration.hosted.base_url)
            .unwrap_or_default(),
        server_url: None,
        job: state.job(id).await,
        summary,
    })
}

async fn dispatch(state: &Arc<AppState>, command: &str, args: Value) -> Result<Value> {
    let value = match command {
        // The browser UI asks on load; there is never a recording in progress here.
        "recorder_status" => json!({
            "recording": false, "sessionId": null, "startedAt": null,
            "eventCount": 0, "microphone": { "state": "off" }, "lastSessionId": null
        }),

        "list_sessions" => to_value(skillrec_core::session::list_sessions()?)?,
        "load_session" => {
            let a: WithId = arg(&args)?;
            to_value(detail(state, &a.id).await?)?
        }
        "delete_session" => {
            let a: WithId = arg(&args)?;
            skillrec_core::session::delete_session(&a.id)?;
            state.plans.lock().await.remove(&a.id);
            state.jobs.lock().await.remove(&a.id);
            Value::Null
        }
        "read_frame" => {
            #[derive(Deserialize)]
            struct A {
                id: String,
                file: String,
            }
            use base64::Engine;
            let a: A = arg(&args)?;
            let dir = skillrec_core::paths::session_dir(&a.id)?;
            let path = skillrec_core::paths::resolve_within(&dir, &a.file)?;
            let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            json!(format!(
                "data:image/jpeg;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            ))
        }

        "get_settings" => to_value(state.config.read().await.settings.clone())?,
        "save_settings" => {
            #[derive(Deserialize)]
            struct A {
                settings: Settings,
            }
            let a: A = arg(&args)?;
            state.config.write().await.settings = a.settings.clone();
            state.save_config().await?;
            to_value(a.settings)?
        }
        "test_connection" => {
            #[derive(Deserialize)]
            struct A {
                settings: Option<Settings>,
            }
            let a: A = arg(&args)?;
            let llm = match a.settings {
                Some(settings) => settings.llm,
                None => state.config.read().await.settings.llm.clone(),
            };
            to_value(LlmClient::new(llm)?.test_connection().await)?
        }

        "analyze_session" => {
            let a: WithId = arg(&args)?;
            let dir = skillrec_core::paths::session_dir(&a.id)?;
            anyhow::ensure!(
                !skillrec_narration::transcribe::needs_transcription(&dir),
                "This recording has narration that has not been transcribed yet. Transcribe it first."
            );
            let llm = state.config.read().await.settings.llm.clone();
            let data = SessionData::load(&dir)?;
            to_value(Describer::new(llm).analyze(data, &jobs::progress(state)).await?)?
        }
        "revise_analysis" => {
            #[derive(Deserialize)]
            struct A {
                id: String,
                feedback: String,
            }
            let a: A = arg(&args)?;
            let dir = skillrec_core::paths::session_dir(&a.id)?;
            let llm = state.config.read().await.settings.llm.clone();
            let data = SessionData::load(&dir)?;
            to_value(Describer::new(llm).revise(data, &a.feedback, &jobs::progress(state)).await?)?
        }
        "edit_analysis" => {
            #[derive(Deserialize)]
            struct A {
                id: String,
                title: Option<String>,
                intent: Option<String>,
                steps: Option<Vec<AnalysisStep>>,
            }
            let a: A = arg(&args)?;
            let dir = skillrec_core::paths::session_dir(&a.id)?;
            let mut analysis: Analysis = read_json(&dir.join("analysis.json"))
                .context("there is no analysis to edit yet")?;
            analysis.apply_edit(a.title, a.intent, a.steps);
            write_json(&dir.join("analysis.json"), &analysis)?;
            if let Err(err) = set_session_title(&dir, Some(&analysis.title)) {
                tracing::warn!("could not update the recording's title: {err:#}");
            }
            to_value(analysis)?
        }
        "debrief_questions" => {
            let a: WithId = arg(&args)?;
            let dir = skillrec_core::paths::session_dir(&a.id)?;
            let llm = state.config.read().await.settings.llm.clone();
            let data = SessionData::load(&dir)?;
            let questions = Debriefer::new(llm).ask(data, &jobs::progress(state)).await?;
            let mut analysis: Analysis = read_json(&dir.join("analysis.json"))
                .context("analyse this recording before debriefing it")?;
            analysis.set_open_questions(questions);
            write_json(&dir.join("analysis.json"), &analysis)?;
            to_value(analysis)?
        }
        "answer_debrief" => {
            #[derive(Deserialize)]
            struct A {
                id: String,
                answers: Vec<DebriefAnswer>,
            }
            let a: A = arg(&args)?;
            let dir = skillrec_core::paths::session_dir(&a.id)?;
            let mut analysis: Analysis = read_json(&dir.join("analysis.json"))
                .context("there is no analysis to answer for yet")?;
            analysis.answer_debrief(&a.answers);
            write_json(&dir.join("analysis.json"), &analysis)?;
            to_value(analysis)?
        }

        "plan_skill" => {
            #[derive(Deserialize)]
            struct A {
                id: String,
                feedback: Option<String>,
            }
            let a: A = arg(&args)?;
            let dir = skillrec_core::paths::session_dir(&a.id)?;
            let llm = state.config.read().await.settings.llm.clone();
            let data = SessionData::load(&dir)?;
            let previous = state.plans.lock().await.get(&a.id).cloned();
            let plan = SkillBuilder::new(llm)
                .plan(data, previous.as_ref(), a.feedback.as_deref(), &jobs::progress(state))
                .await?;
            state.plans.lock().await.insert(a.id, plan.clone());
            to_value(plan)?
        }
        "build_skill" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct A {
                id: String,
                values: Vec<FixedValue>,
                export_dir: Option<String>,
            }
            let a: A = arg(&args)?;
            let dir = skillrec_core::paths::session_dir(&a.id)?;
            let llm = state.config.read().await.settings.llm.clone();
            let mut plan = state
                .plans
                .lock()
                .await
                .get(&a.id)
                .cloned()
                .context("propose a plan before building the skill")?;
            skillrec_agent::builder::apply_value_edits(&mut plan, &a.values);
            // The browser has no file picker onto the server's disk: skills are
            // installed under the server's data folder and shown in the UI.
            let target = match a.export_dir {
                Some(dir) => SkillTarget::Export(dir.into()),
                None => SkillTarget::Install,
            };
            let data = SessionData::load(&dir)?;
            let (skill, path) = SkillBuilder::new(llm)
                .build(data, &plan, target, &jobs::progress(state))
                .await?;
            json!({ "skill": skill, "path": path.display().to_string() })
        }

        "whisper_status" => {
            let model = state.config.read().await.settings.narration.model;
            json!({
                "model": model,
                "cached": skillrec_narration::is_model_cached(model),
                "approxMb": model.approx_mb(),
            })
        }
        "download_whisper_model" => {
            let model = state.config.read().await.settings.narration.model;
            let sink = {
                let state = Arc::clone(state);
                move |p: skillrec_narration::DownloadProgress| state.emit("whisper://download", &p)
            };
            json!(skillrec_narration::ensure_model(model, &sink).await?.display().to_string())
        }
        "transcribe_session" => {
            let a: WithId = arg(&args)?;
            let dir = skillrec_core::paths::session_dir(&a.id)?;
            let narration = state.config.read().await.settings.narration.clone();
            to_value(jobs::transcribe(state, &a.id, &dir, &narration).await?)?
        }

        "app_info" => json!({
            "name": "TeachOnce Server",
            "version": env!("CARGO_PKG_VERSION"),
            "identifier": "ai.teachonce.server",
            "dataDir": state.data_dir.display().to_string(),
            "skillsDir": skillrec_core::paths::skills_root()?.display().to_string(),
            "author": "Roberto Moreno",
            "repository": "https://github.com/robertoamoreno/teachonce",
            "license": "MIT",
        }),
        "server_info" => json!({
            "version": env!("CARGO_PKG_VERSION"),
            "dataDir": state.data_dir.display().to_string(),
            "apiKey": state.config.read().await.api_key.clone(),
            "sessions": skillrec_core::session::list_sessions().map(|s| s.len()).unwrap_or(0),
        }),
        "rotate_api_key" => {
            let key = new_api_key();
            state.config.write().await.api_key = key.clone();
            state.save_config().await?;
            tracing::info!("API key rotated");
            json!({ "apiKey": key })
        }
        "list_jobs" => to_value(state.jobs.lock().await.values().cloned().collect::<Vec<_>>())?,
        "process_session" => {
            let a: WithId = arg(&args)?;
            skillrec_core::paths::session_dir(&a.id)?;
            state.set_job(&a.id, "received", "Queued.").await;
            jobs::spawn_pipeline(Arc::clone(state), a.id);
            json!({ "queued": true })
        }

        "start_recording" | "stop_recording" | "discard_recording" | "toggle_recording"
        | "set_microphone" | "list_microphones" | "list_displays" | "permission_report"
        | "request_screen_recording" | "submit_session" | "test_server" => {
            anyhow::bail!("{command} runs only in the desktop app")
        }
        other => anyhow::bail!("unknown command {other}"),
    };
    Ok(value)
}
