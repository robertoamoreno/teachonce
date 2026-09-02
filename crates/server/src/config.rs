//! `server.json`: the shared API key and the same settings the app keeps.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use skillrec_core::config::Settings;
use skillrec_core::session::write_json;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ServerConfig {
    /// The one shared key. Every client and every browser presents it.
    pub api_key: String,
    /// Model endpoint and narration settings, exactly as the app stores them.
    /// The capture section is carried but unused: the server does not record.
    pub settings: Settings,
}

impl ServerConfig {
    /// Load the file, or create it with a fresh key on the first start.
    ///
    /// A corrupt file is an error rather than a silent reset: replacing it
    /// would rotate the key under every client that has it.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        let mut config = if path.exists() {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?
        } else {
            Self::default()
        };
        if config.api_key.trim().is_empty() {
            config.api_key = new_api_key();
        }
        config.save(path)?;
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_json(path, self)
    }
}

/// `tk_` plus 32 hex characters: unguessable, and unmistakable in a log.
pub fn new_api_key() -> String {
    format!("tk_{}", uuid::Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_first_start_creates_the_file_with_a_key_and_a_restart_keeps_it() {
        let dir = std::env::temp_dir().join(format!("teachonce-server-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.json");

        let first = ServerConfig::load_or_create(&path).unwrap();
        assert!(first.api_key.starts_with("tk_"));
        assert_eq!(first.api_key.len(), 3 + 32);
        assert!(path.exists());

        let second = ServerConfig::load_or_create(&path).unwrap();
        assert_eq!(second.api_key, first.api_key, "a restart must not rotate the key");

        std::fs::write(&path, "{ not json").unwrap();
        assert!(ServerConfig::load_or_create(&path).is_err(), "a corrupt file is reported, not reset");
        std::fs::remove_dir_all(&dir).ok();
    }
}
