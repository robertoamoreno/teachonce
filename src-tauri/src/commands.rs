//! The command surface the UI calls.
//!
//! Errors are stringified at this boundary because that is what crosses the IPC
//! bridge, but the message is the full `anyhow` chain (`{:#}`) — a user seeing
//! "could not reach http://localhost:11434/v1: connection refused" can fix their
//! problem, where "analysis failed" leaves them guessing.

use serde::Serialize;
use skillrec_agent::{Debriefer, Describer, SessionData, SkillBuilder, SkillTarget};
use skillrec_capture::audio::MicrophoneDevice;
use skillrec_capture::permissions::{self, PermissionReport};
use skillrec_core::analysis::{Analysis, AnalysisStep, DebriefAnswer};
use skillrec_core::config::{Settings, TranscriptionBackend, WhisperModel};
use skillrec_core::session::{set_session_title, write_json, SessionSummary};
use skillrec_core::skill::{BuiltSkill, FixedValue, SkillPlan};
use skillrec_recorder::{MicrophoneState, RecorderStatus};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::state::AppState;

type Reply<T> = Result<T, String>;

/// Render an error chain for the UI.
fn fail(err: impl std::fmt::Display) -> String {
    format!("{err:#}")
}

// --- Recording ---------------------------------------------------------------

#[tauri::command]
pub async fn recorder_status(state: State<'_, AppState>) -> Reply<RecorderStatus> {
    Ok(state.recorder.status().await)
}

#[tauri::command]
pub async fn start_recording(
    app: AppHandle,
    narrate: bool,
    device: Option<String>,
    state: State<'_, AppState>,
) -> Reply<String> {
    let capture = state.settings.lock().await.capture;
    let id = state.recorder.start(capture, narrate, device).await.map_err(fail)?;
    crate::emit_status(&app, &state.recorder.status().await);
    Ok(id)
}

#[tauri::command]
pub async fn stop_recording(app: AppHandle, state: State<'_, AppState>) -> Reply<String> {
    let id = state.recorder.stop().await.map_err(fail)?;
    crate::emit_status(&app, &state.recorder.status().await);
    let _ = app.emit("recorder://saved", &id);
    Ok(id)
}

#[tauri::command]
pub async fn discard_recording(app: AppHandle, state: State<'_, AppState>) -> Reply<String> {
    let id = state.recorder.discard().await.map_err(fail)?;
    crate::emit_status(&app, &state.recorder.status().await);
    Ok(id)
}

/// Start or stop, whichever applies. Backs the tray item and the hotkey.
#[tauri::command]
pub async fn toggle_recording(app: AppHandle) -> Reply<bool> {
    let state = app.state::<AppState>();
    if state.recorder.is_recording().await {
        let id = state.recorder.stop().await.map_err(fail)?;
        crate::emit_status(&app, &state.recorder.status().await);
        // Same as the Stop button: the library jumps to what was just saved.
        let _ = app.emit("recorder://saved", &id);
        Ok(false)
    } else {
        let capture = state.settings.lock().await.capture;
        state.recorder.start(capture, false, None).await.map_err(fail)?;
        crate::emit_status(&app, &state.recorder.status().await);
        Ok(true)
    }
}

#[tauri::command]
pub async fn set_microphone(
    app: AppHandle,
    on: bool,
    device: Option<String>,
    state: State<'_, AppState>,
) -> Reply<MicrophoneState> {
    let result = state.recorder.set_microphone(on, device).await.map_err(fail)?;
    crate::emit_status(&app, &state.recorder.status().await);
    Ok(result)
}

#[tauri::command]
pub fn list_microphones() -> Vec<MicrophoneDevice> {
    skillrec_capture::audio::list_microphones()
}

// --- Permissions -------------------------------------------------------------

#[tauri::command]
pub fn permission_report() -> PermissionReport {
    permissions::report()
}

#[tauri::command]
pub fn request_screen_recording() -> bool {
    permissions::request_screen_recording()
}

// --- Library -----------------------------------------------------------------

#[tauri::command]
pub fn list_sessions() -> Reply<Vec<SessionSummary>> {
    skillrec_core::session::list_sessions().map_err(fail)
}

/// Everything one recording holds, for the detail view.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDetail {
    pub summary: SessionSummary,
    pub description: String,
    pub timeline: serde_json::Value,
    pub narration: Option<skillrec_core::narration::NarrationTranscript>,
    pub analysis: Option<Analysis>,
    pub skill: Option<BuiltSkill>,
    pub frames: Vec<skillrec_core::frames::FrameRecord>,
    pub needs_transcription: bool,
    /// Where Transcribe would send the audio, so the button can say so before
    /// anything leaves the machine.
    pub transcribe_via: TranscriptionBackend,
    /// Host of the hosted endpoint, for the same button.
    pub transcribe_host: String,
}

#[tauri::command]
pub async fn load_session(id: String, state: State<'_, AppState>) -> Reply<SessionDetail> {
    let dir = skillrec_core::paths::session_dir(&id).map_err(fail)?;
    let data = SessionData::load(&dir).map_err(fail)?;
    let summary = skillrec_core::session::list_sessions()
        .map_err(fail)?
        .into_iter()
        .find(|s| s.meta.id == id)
        .ok_or_else(|| format!("no recording called {id}"))?;
    let narration = state.settings.lock().await.narration.clone();

    Ok(SessionDetail {
        description: std::fs::read_to_string(dir.join("description.md")).unwrap_or_default(),
        timeline: serde_json::to_value(data.timeline_view()).unwrap_or_default(),
        narration: data.narration.clone(),
        analysis: data.analysis.clone(),
        skill: skillrec_core::session::read_json(&dir.join("skill.json")),
        frames: data.frames.frames.clone(),
        needs_transcription: skillrec_narration::transcribe::needs_transcription(&dir),
        transcribe_via: narration.backend,
        transcribe_host: skillrec_core::timeline::host_of(&narration.hosted.base_url)
            .unwrap_or_default(),
        summary,
    })
}

#[tauri::command]
pub fn delete_session(id: String) -> Reply<()> {
    skillrec_core::session::delete_session(&id).map_err(fail)
}

/// Read a frame as a data URL, for the detail view's filmstrip.
#[tauri::command]
pub fn read_frame(id: String, file: String) -> Reply<String> {
    use base64::Engine;
    let dir = skillrec_core::paths::session_dir(&id).map_err(fail)?;
    // Goes through the same guard the model's tools use: a frame path is data,
    // and data does not get to name arbitrary files.
    let path = skillrec_core::paths::resolve_within(&dir, &file).map_err(fail)?;
    let bytes = std::fs::read(&path).map_err(fail)?;
    Ok(format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

// --- Settings ----------------------------------------------------------------

#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> Reply<Settings> {
    Ok(state.settings.lock().await.clone())
}

#[tauri::command]
pub async fn save_settings(settings: Settings, state: State<'_, AppState>) -> Reply<Settings> {
    settings.save().map_err(fail)?;
    *state.settings.lock().await = settings.clone();
    Ok(settings)
}

#[tauri::command]
pub async fn test_connection(
    settings: Option<Settings>,
    state: State<'_, AppState>,
) -> Reply<skillrec_agent::ConnectionTest> {
    // Test what is in the form, not what was last saved — otherwise the button
    // cannot tell you whether your edit is correct.
    let config = match settings {
        Some(settings) => settings.llm,
        None => state.settings.lock().await.llm.clone(),
    };
    let client = skillrec_agent::LlmClient::new(config).map_err(fail)?;
    Ok(client.test_connection().await)
}

// --- Analysis ----------------------------------------------------------------

/// Emit agent progress on a channel the UI subscribes to.
fn progress_sink(app: &AppHandle) -> impl Fn(skillrec_agent::AgentProgress) + Send + Sync + use<> {
    let app = app.clone();
    move |progress| {
        let _ = app.emit("agent://progress", &progress);
    }
}

#[tauri::command]
pub async fn analyze_session(
    app: AppHandle,
    id: String,
    state: State<'_, AppState>,
) -> Reply<Analysis> {
    let dir = skillrec_core::paths::session_dir(&id).map_err(fail)?;

    // Narration is the user's own statement of intent. Analysing without it when
    // it exists but has not been transcribed would quietly throw away the best
    // signal in the recording.
    if skillrec_narration::transcribe::needs_transcription(&dir) {
        return Err(
            "This recording has narration that has not been transcribed yet. Transcribe it first."
                .into(),
        );
    }

    let config = state.settings.lock().await.llm.clone();
    let data = SessionData::load(&dir).map_err(fail)?;
    Describer::new(config)
        .analyze(data, &progress_sink(&app))
        .await
        .map_err(fail)
}

#[tauri::command]
pub async fn revise_analysis(
    app: AppHandle,
    id: String,
    feedback: String,
    state: State<'_, AppState>,
) -> Reply<Analysis> {
    let dir = skillrec_core::paths::session_dir(&id).map_err(fail)?;
    let config = state.settings.lock().await.llm.clone();
    let data = SessionData::load(&dir).map_err(fail)?;
    Describer::new(config)
        .revise(data, &feedback, &progress_sink(&app))
        .await
        .map_err(fail)
}

/// Apply a direct user edit. The model is not involved.
#[tauri::command]
pub fn edit_analysis(
    id: String,
    title: Option<String>,
    intent: Option<String>,
    steps: Option<Vec<AnalysisStep>>,
) -> Reply<Analysis> {
    let dir = skillrec_core::paths::session_dir(&id).map_err(fail)?;
    let mut analysis: Analysis = skillrec_core::session::read_json(&dir.join("analysis.json"))
        .ok_or("there is no analysis to edit yet")?;
    analysis.apply_edit(title, intent, steps);
    write_json(&dir.join("analysis.json"), &analysis).map_err(fail)?;
    if let Err(err) = set_session_title(&dir, Some(&analysis.title)) {
        tracing::warn!("could not update the recording's title: {err:#}");
    }
    Ok(analysis)
}

// --- Debrief -----------------------------------------------------------------

/// Ask up to five questions the recording cannot answer, and store them on the
/// analysis as open questions. Anything the user already answered is kept.
#[tauri::command]
pub async fn debrief_questions(
    app: AppHandle,
    id: String,
    state: State<'_, AppState>,
) -> Reply<Analysis> {
    let dir = skillrec_core::paths::session_dir(&id).map_err(fail)?;
    let config = state.settings.lock().await.llm.clone();
    let data = SessionData::load(&dir).map_err(fail)?;
    let questions = Debriefer::new(config)
        .ask(data, &progress_sink(&app))
        .await
        .map_err(fail)?;

    // Re-read rather than reuse the loaded copy: the user may have answered
    // or edited while the model was thinking.
    let mut analysis: Analysis = skillrec_core::session::read_json(&dir.join("analysis.json"))
        .ok_or("analyse this recording before debriefing it")?;
    analysis.set_open_questions(questions);
    write_json(&dir.join("analysis.json"), &analysis).map_err(fail)?;
    Ok(analysis)
}

/// Record the user's answers (or skips). The model is not involved.
#[tauri::command]
pub fn answer_debrief(id: String, answers: Vec<DebriefAnswer>) -> Reply<Analysis> {
    let dir = skillrec_core::paths::session_dir(&id).map_err(fail)?;
    let mut analysis: Analysis = skillrec_core::session::read_json(&dir.join("analysis.json"))
        .ok_or("there is no analysis to answer for yet")?;
    analysis.answer_debrief(&answers);
    write_json(&dir.join("analysis.json"), &analysis).map_err(fail)?;
    Ok(analysis)
}

// --- Building ----------------------------------------------------------------

#[tauri::command]
pub async fn plan_skill(
    app: AppHandle,
    id: String,
    feedback: Option<String>,
    state: State<'_, AppState>,
) -> Reply<SkillPlan> {
    let dir = skillrec_core::paths::session_dir(&id).map_err(fail)?;
    let config = state.settings.lock().await.llm.clone();
    let data = SessionData::load(&dir).map_err(fail)?;

    let previous = state.plans.lock().await.get(&id).cloned();
    let plan = SkillBuilder::new(config)
        .plan(data, previous.as_ref(), feedback.as_deref(), &progress_sink(&app))
        .await
        .map_err(fail)?;

    state.plans.lock().await.insert(id, plan.clone());
    Ok(plan)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildResult {
    pub skill: BuiltSkill,
    pub path: String,
}

#[tauri::command]
pub async fn build_skill(
    app: AppHandle,
    id: String,
    values: Vec<FixedValue>,
    export_dir: Option<String>,
    state: State<'_, AppState>,
) -> Reply<BuildResult> {
    let dir = skillrec_core::paths::session_dir(&id).map_err(fail)?;
    let config = state.settings.lock().await.llm.clone();

    let mut plan = state
        .plans
        .lock()
        .await
        .get(&id)
        .cloned()
        .ok_or("propose a plan before building the skill")?;
    // The user's edits are applied to the plan the model built, so a value they
    // retargeted is what gets written — the model does not get a second say.
    skillrec_agent::builder::apply_value_edits(&mut plan, &values);

    let target = match export_dir {
        Some(dir) => SkillTarget::Export(dir.into()),
        None => SkillTarget::Install,
    };

    let data = SessionData::load(&dir).map_err(fail)?;
    let (skill, path) = SkillBuilder::new(config)
        .build(data, &plan, target, &progress_sink(&app))
        .await
        .map_err(fail)?;

    Ok(BuildResult { skill, path: path.display().to_string() })
}

// --- About -------------------------------------------------------------------

/// What the About panel shows: the facts a user needs to find their data, name
/// the build in a bug report, and know where the app's edges are.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub name: String,
    pub version: String,
    pub identifier: String,
    pub data_dir: String,
    pub skills_dir: String,
    pub author: String,
    pub repository: String,
    pub license: String,
}

#[tauri::command]
pub fn app_info(app: AppHandle) -> Reply<AppInfo> {
    let package = app.package_info();
    Ok(AppInfo {
        name: package.name.clone(),
        version: package.version.to_string(),
        identifier: app.config().identifier.clone(),
        data_dir: skillrec_core::paths::data_root().map_err(fail)?.display().to_string(),
        skills_dir: skillrec_core::paths::skills_root().map_err(fail)?.display().to_string(),
        author: "Roberto Moreno".into(),
        repository: "https://github.com/robertoamoreno/teachonce".into(),
        license: "MIT".into(),
    })
}

// --- Narration ---------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WhisperStatus {
    pub model: WhisperModel,
    pub cached: bool,
    pub approx_mb: u32,
}

#[tauri::command]
pub async fn whisper_status(state: State<'_, AppState>) -> Reply<WhisperStatus> {
    let model = state.settings.lock().await.narration.model;
    Ok(WhisperStatus {
        cached: skillrec_narration::is_model_cached(model),
        approx_mb: model.approx_mb(),
        model,
    })
}

#[tauri::command]
pub async fn download_whisper_model(app: AppHandle, state: State<'_, AppState>) -> Reply<String> {
    let model = state.settings.lock().await.narration.model;
    let sink = {
        let app = app.clone();
        move |progress: skillrec_narration::DownloadProgress| {
            let _ = app.emit("whisper://download", &progress);
        }
    };
    let path = skillrec_narration::ensure_model(model, &sink).await.map_err(fail)?;
    Ok(path.display().to_string())
}

#[tauri::command]
pub async fn transcribe_session(
    app: AppHandle,
    id: String,
    state: State<'_, AppState>,
) -> Reply<skillrec_core::narration::NarrationTranscript> {
    let dir = skillrec_core::paths::session_dir(&id).map_err(fail)?;
    let config = state.settings.lock().await.narration.clone();

    let transcript = match config.backend {
        TranscriptionBackend::Local => {
            let sink = {
                let app = app.clone();
                move |progress: skillrec_narration::DownloadProgress| {
                    let _ = app.emit("whisper://download", &progress);
                }
            };
            let weights =
                skillrec_narration::ensure_model(config.model, &sink).await.map_err(fail)?;

            // Whisper is a long CPU/GPU-bound call. Running it on a blocking
            // thread keeps the UI and the async runtime responsive throughout.
            tokio::task::spawn_blocking(move || {
                skillrec_narration::transcribe_session(&dir, &config, &weights)
            })
            .await
            .map_err(fail)?
            .map_err(fail)?
        }
        TranscriptionBackend::Hosted => {
            // The only path on which audio leaves the machine, and the user
            // chose it in Settings. Progress rides the same channel the agents
            // use, so the library's progress line shows the uploads.
            let sink = {
                let app = app.clone();
                let session_id = id.clone();
                move |message: String| {
                    let _ = app.emit(
                        "agent://progress",
                        &skillrec_agent::AgentProgress {
                            session_id: session_id.clone(),
                            phase: "transcribe".into(),
                            message,
                        },
                    );
                }
            };
            skillrec_narration::transcribe_session_hosted(&dir, &config, &sink)
                .await
                .map_err(fail)?
        }
    };

    let _ = app.emit("narration://done", &id);
    Ok(transcript)
}
