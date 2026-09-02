//! Shared server state: configuration, draft plans, job status, and the event
//! stream browsers subscribe to.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use skillrec_core::clock::epoch_ms;
use skillrec_core::skill::SkillPlan;
use tokio::sync::{broadcast, Mutex, RwLock, Semaphore};

use crate::config::ServerConfig;

/// One event for the browser, mirroring what the desktop app emits through
/// Tauri: `agent://progress`, `whisper://download`, `job://status`, …
#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub event: String,
    pub payload: Value,
}

/// Where a recording is in the server-side pipeline.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobStatus {
    pub id: String,
    /// `received`, `reconstructing`, `transcribing`, `analysing`,
    /// `debriefing`, `done` or `failed`.
    pub phase: String,
    pub message: String,
    pub updated_at: i64,
}

pub struct AppState {
    pub data_dir: PathBuf,
    pub config_path: PathBuf,
    pub config: RwLock<ServerConfig>,
    /// Draft plans under review, per session — in memory, like the app.
    pub plans: Mutex<HashMap<String, SkillPlan>>,
    pub jobs: Mutex<HashMap<String, JobStatus>>,
    pub events: broadcast::Sender<Event>,
    /// One model pipeline at a time: a local endpoint serialises anyway, and
    /// two analyses racing for it just makes both slow.
    pub pipeline: Semaphore,
}

impl AppState {
    pub fn new(data_dir: PathBuf, config_path: PathBuf, config: ServerConfig) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            data_dir,
            config_path,
            config: RwLock::new(config),
            plans: Mutex::new(HashMap::new()),
            jobs: Mutex::new(HashMap::new()),
            events,
            pipeline: Semaphore::new(1),
        }
    }

    /// Broadcast an event. No subscribers is not an error.
    pub fn emit(&self, event: &str, payload: &impl Serialize) {
        let payload = serde_json::to_value(payload).unwrap_or(Value::Null);
        let _ = self.events.send(Event { event: event.to_string(), payload });
    }

    pub async fn save_config(&self) -> Result<()> {
        self.config.read().await.save(&self.config_path)
    }

    pub async fn set_job(&self, id: &str, phase: &str, message: impl Into<String>) {
        let status = JobStatus {
            id: id.to_string(),
            phase: phase.to_string(),
            message: message.into(),
            updated_at: epoch_ms(),
        };
        self.jobs.lock().await.insert(id.to_string(), status.clone());
        self.emit("job://status", &status);
    }

    pub async fn job(&self, id: &str) -> Option<JobStatus> {
        self.jobs.lock().await.get(id).cloned()
    }
}
