//! Capture and model configuration.
//!
//! The privacy boundary of this app is a single sentence: **capture, storage,
//! frame selection and transcription never leave the machine; the analysis step
//! talks to whatever OpenAI-compatible endpoint you configure.** [`LlmConfig`] is
//! that one outbound door, which is why it is a small, explicit, inspectable
//! struct rather than an ambient environment variable read from ten places.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Which signals the recorder collects. Every one is on by default; the point of
/// the struct is that a user can turn a source *off* and it is then never
/// constructed — so a disabled source costs nothing and, crucially on macOS,
/// never triggers its permission prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CaptureConfig {
    /// Foreground app switches.
    pub app_activity: bool,
    /// Window and document titles (needs Accessibility).
    pub window_titles: bool,
    /// Active browser tab URLs (needs per-browser Automation).
    pub browser_urls: bool,
    /// Clipboard copies — formats, length, hash, short preview.
    pub clipboard: bool,
    /// Periodic screen stills, kept only on change (needs Screen Recording).
    pub screen_frames: bool,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            app_activity: true,
            window_titles: true,
            browser_urls: true,
            clipboard: true,
            screen_frames: true,
        }
    }
}

/// Connection to an OpenAI-compatible chat-completions endpoint.
///
/// Anything that speaks the standard works unchanged: Ollama
/// (`http://localhost:11434/v1`), LM Studio (`http://localhost:1234/v1`),
/// llama.cpp's server, vLLM, OpenRouter, or api.openai.com itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LlmConfig {
    /// Base URL **including** the `/v1` suffix.
    pub base_url: String,
    /// Model id as the server names it.
    pub model: String,
    /// Sent as `Authorization: Bearer …`. Local servers usually ignore it, but
    /// most still require the header to be present, so we default to a dummy.
    pub api_key: String,
    /// Whether this model accepts image parts in a user message. When false the
    /// describer keeps its frame tools hidden and works from events + narration
    /// alone, rather than calling a tool the server will reject.
    pub vision: bool,
    /// Sampling temperature for analysis turns. Low by default: this is a
    /// reconstruction task, not a creative one.
    pub temperature: f32,
    /// Ceiling on a single response.
    pub max_tokens: u32,
    /// Per-request timeout in seconds. Generous, because a local 7B model on a
    /// laptop can take a while on a long tool-call turn.
    pub request_timeout_secs: u64,
    /// Sent as `reasoning_effort` unless it is `default`. Thinking models such
    /// as qwen3 spend most of a turn reasoning; `none` makes them answer at
    /// once, which on a laptop is the difference between five seconds and three
    /// minutes. A server that rejects the field is detected and it is dropped.
    pub reasoning_effort: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:11434/v1".into(),
            // Tool calling matters more than vision here: most steps are fully
            // explained by events and narration, and a model with no native
            // tool support has to fall back to prompted tools, which is slower
            // and less reliable. Ollama's vision models mostly lack tools, so
            // the default favours the tool-capable one and leaves vision off.
            model: "qwen3:8b".into(),
            api_key: "sk-no-key-required".into(),
            vision: false,
            temperature: 0.1,
            max_tokens: 4096,
            request_timeout_secs: 300,
            reasoning_effort: "default".into(),
        }
    }
}

impl LlmConfig {
    /// The `reasoning_effort` value to send, if any.
    pub fn reasoning_effort_to_send(&self) -> Option<&str> {
        let value = self.reasoning_effort.trim();
        (!value.is_empty() && !value.eq_ignore_ascii_case("default")).then_some(value)
    }

    /// Full URL of the chat-completions endpoint.
    pub fn chat_completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    /// Full URL of the models endpoint, used by the connection test.
    pub fn models_url(&self) -> String {
        format!("{}/models", self.base_url.trim_end_matches('/'))
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(!self.model.trim().is_empty(), "no model id is configured");
        let url = self.base_url.trim();
        anyhow::ensure!(!url.is_empty(), "no base URL is configured");
        anyhow::ensure!(
            url.starts_with("http://") || url.starts_with("https://"),
            "the base URL must start with http:// or https://"
        );
        Ok(())
    }
}

/// Which Whisper checkpoint transcribes narration, all of them running locally
/// through whisper.cpp. Larger is more accurate and slower; `small` matches the
/// upstream app's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WhisperModel {
    Tiny,
    Base,
    /// The default: the accuracy knee for narration, at a 466 MB download.
    #[default]
    Small,
    Medium,
    LargeV3Turbo,
}

impl WhisperModel {
    /// The ggml filename this checkpoint is published under.
    pub fn file_name(self) -> &'static str {
        match self {
            Self::Tiny => "ggml-tiny.bin",
            Self::Base => "ggml-base.bin",
            Self::Small => "ggml-small.bin",
            Self::Medium => "ggml-medium.bin",
            Self::LargeV3Turbo => "ggml-large-v3-turbo.bin",
        }
    }

    /// Where the weights are fetched from on first use.
    pub fn download_url(self) -> String {
        format!(
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{}",
            self.file_name()
        )
    }

    /// Approximate download size, shown before the one-time fetch is started.
    pub fn approx_mb(self) -> u32 {
        match self {
            Self::Tiny => 75,
            Self::Base => 142,
            Self::Small => 466,
            Self::Medium => 1_500,
            Self::LargeV3Turbo => 1_620,
        }
    }
}

/// Narration settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct NarrationConfig {
    pub model: WhisperModel,
    /// ISO-639-1 code, or `auto` to let Whisper detect it.
    pub language: String,
}

impl Default for NarrationConfig {
    fn default() -> Self {
        Self { model: WhisperModel::default(), language: "auto".into() }
    }
}

/// Everything persisted in `settings.json`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub capture: CaptureConfig,
    pub llm: LlmConfig,
    pub narration: NarrationConfig,
}

impl Settings {
    /// Load from disk, falling back to defaults when the file is absent.
    ///
    /// A *corrupt* file is a different matter and is reported: silently reverting
    /// to defaults would quietly point analysis at a different endpoint than the
    /// user configured.
    pub fn load() -> Result<Self> {
        let file = crate::paths::settings_file()?;
        if !file.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", file.display()))
    }

    /// Write atomically, so a crash mid-write can't truncate the settings.
    pub fn save(&self) -> Result<()> {
        let file = crate::paths::settings_file()?;
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = file.with_extension(format!("json.tmp.{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &file)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_urls_tolerate_a_trailing_slash() {
        let cfg = LlmConfig { base_url: "http://localhost:1234/v1/".into(), ..Default::default() };
        assert_eq!(cfg.chat_completions_url(), "http://localhost:1234/v1/chat/completions");
        assert_eq!(cfg.models_url(), "http://localhost:1234/v1/models");
    }

    #[test]
    fn validation_rejects_a_url_without_a_scheme() {
        let cfg = LlmConfig { base_url: "localhost:1234/v1".into(), ..Default::default() };
        assert!(cfg.validate().is_err());
        let cfg = LlmConfig { model: "  ".into(), ..Default::default() };
        assert!(cfg.validate().is_err());
        assert!(LlmConfig::default().validate().is_ok());
    }

    #[test]
    fn settings_round_trip_as_camel_case_json() {
        let json = serde_json::to_string(&Settings::default()).unwrap();
        assert!(json.contains("\"browserUrls\""));
        assert!(json.contains("\"baseUrl\""));
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Settings::default());
    }

    #[test]
    fn partial_settings_files_fill_in_defaults() {
        // Forward compatibility: an older settings.json missing whole sections
        // must still load rather than bricking the app.
        let s: Settings = serde_json::from_str(r#"{"llm":{"model":"gpt-4o-mini"}}"#).unwrap();
        assert_eq!(s.llm.model, "gpt-4o-mini");
        assert_eq!(s.llm.base_url, LlmConfig::default().base_url);
        assert!(s.capture.clipboard);
        // A settings file from before the field existed sends nothing.
        assert_eq!(s.llm.reasoning_effort_to_send(), None);
    }

    #[test]
    fn reasoning_effort_is_only_sent_when_set_to_something() {
        for value in ["default", "Default", "", "  "] {
            let cfg = LlmConfig { reasoning_effort: value.into(), ..Default::default() };
            assert_eq!(cfg.reasoning_effort_to_send(), None, "{value:?} must not be sent");
        }
        let cfg = LlmConfig { reasoning_effort: " none ".into(), ..Default::default() };
        assert_eq!(cfg.reasoning_effort_to_send(), Some("none"));
    }
}
