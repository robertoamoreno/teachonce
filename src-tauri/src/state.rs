//! Shared application state.

use std::sync::Arc;

use skillrec_core::config::Settings;
use skillrec_core::skill::SkillPlan;
use skillrec_recorder::Recorder;
use tokio::sync::Mutex;

pub struct AppState {
    pub recorder: Arc<Recorder>,
    /// Cached settings, so every command does not re-read the file. Writes go
    /// through `save_settings`, which updates both this and the disk copy.
    pub settings: Mutex<Settings>,
    /// The plan currently under review, per session.
    ///
    /// Held in memory rather than on disk because it is a *draft*: it only
    /// becomes an artifact when the user approves it and the skill is built.
    pub plans: Mutex<std::collections::HashMap<String, SkillPlan>>,
}

impl AppState {
    pub fn new(app_version: String) -> Self {
        let settings = Settings::load().unwrap_or_else(|err| {
            // A corrupt settings file must be visible, not silently replaced —
            // it could mean analysis is pointing somewhere unexpected.
            tracing::warn!("could not load settings, using defaults: {err:#}");
            Settings::default()
        });
        Self {
            recorder: Arc::new(Recorder::new(app_version)),
            settings: Mutex::new(settings),
            plans: Mutex::new(std::collections::HashMap::new()),
        }
    }
}
