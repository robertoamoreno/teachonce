//! The pipeline a received recording goes through, and the transcription
//! helper the manual RPC shares with it.
//!
//! Every stage is the same library call the desktop app makes; the only thing
//! the server adds is running them unattended, one recording at a time, with
//! status the browser can watch.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use skillrec_agent::{AgentProgress, Debriefer, Describer, SessionData};
use skillrec_core::analysis::Analysis;
use skillrec_core::config::{NarrationConfig, TranscriptionBackend};
use skillrec_core::narration::NarrationTranscript;
use skillrec_core::session::{read_json, reconstruct_session, write_json};
use skillrec_narration::transcribe::needs_transcription;

use crate::state::AppState;

/// Run the pipeline in the background; failures land in the job status.
pub fn spawn_pipeline(state: Arc<AppState>, id: String) {
    tokio::spawn(async move {
        if let Err(err) = run(&state, &id).await {
            tracing::warn!(%id, "pipeline failed: {err:#}");
            state.set_job(&id, "failed", format!("{err:#}")).await;
        }
    });
}

async fn run(state: &Arc<AppState>, id: &str) -> Result<()> {
    let _permit = state.pipeline.acquire().await.context("the pipeline is closed")?;
    let dir = skillrec_core::paths::session_dir(id)?;
    let settings = state.config.read().await.settings.clone();

    state.set_job(id, "reconstructing", "Rebuilding the timeline…").await;
    reconstruct_session(&dir)?;

    if needs_transcription(&dir) {
        state.set_job(id, "transcribing", "Transcribing narration…").await;
        transcribe(state, id, &dir, &settings.narration).await?;
    }

    if read_json::<Analysis>(&dir.join("analysis.json")).is_none() {
        state.set_job(id, "analysing", "Analysing the recording…").await;
        let data = SessionData::load(&dir)?;
        Describer::new(settings.llm.clone()).analyze(data, &progress(state)).await?;
    }

    let analysis: Analysis = read_json(&dir.join("analysis.json"))
        .context("the analysis was not written")?;
    if analysis.debrief.is_empty() {
        state.set_job(id, "debriefing", "Preparing debrief questions…").await;
        let data = SessionData::load(&dir)?;
        let questions = Debriefer::new(settings.llm.clone()).ask(data, &progress(state)).await?;
        let mut analysis: Analysis = read_json(&dir.join("analysis.json"))
            .context("the analysis disappeared mid-pipeline")?;
        analysis.set_open_questions(questions);
        write_json(&dir.join("analysis.json"), &analysis)?;
    }

    state.set_job(id, "done", "Ready to review.").await;
    Ok(())
}

/// Transcribe a recording's narration with the configured backend, exactly as
/// the desktop app does, with progress on the event stream.
pub async fn transcribe(
    state: &Arc<AppState>,
    id: &str,
    dir: &Path,
    config: &NarrationConfig,
) -> Result<NarrationTranscript> {
    let transcript = match config.backend {
        TranscriptionBackend::Local => {
            let download = {
                let state = Arc::clone(state);
                move |p: skillrec_narration::DownloadProgress| state.emit("whisper://download", &p)
            };
            let weights = skillrec_narration::ensure_model(config.model, &download).await?;
            let dir = dir.to_path_buf();
            let config = config.clone();
            tokio::task::spawn_blocking(move || {
                skillrec_narration::transcribe_session(&dir, &config, &weights)
            })
            .await
            .context("the transcription task failed")??
        }
        TranscriptionBackend::Hosted => {
            let sink = {
                let state = Arc::clone(state);
                let session_id = id.to_string();
                move |message: String| {
                    state.emit(
                        "agent://progress",
                        &AgentProgress {
                            session_id: session_id.clone(),
                            phase: "transcribe".into(),
                            message,
                        },
                    )
                }
            };
            skillrec_narration::transcribe_session_hosted(dir, config, &sink).await?
        }
    };
    state.emit("narration://done", &id);
    Ok(transcript)
}

/// Agent progress onto the event stream.
pub fn progress(state: &Arc<AppState>) -> impl Fn(AgentProgress) + Send + Sync + use<> {
    let state = Arc::clone(state);
    move |p| state.emit("agent://progress", &p)
}
