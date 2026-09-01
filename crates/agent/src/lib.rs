//! The analysis side of Skill Recorder — and the **only** crate in this
//! workspace that opens a network connection.
//!
//! Everything else (capture, frame selection, transcription) runs locally with
//! no way to reach the network. Analysis is opt-in per recording: nothing is
//! sent until the user presses Analyse, and what is sent is exactly what the
//! tools in [`describer`] return — the timeline, the events, the narration text,
//! and any frames the model explicitly asks to look at.
//!
//! The endpoint is whatever you configure. Anything speaking the OpenAI
//! chat-completions API works: Ollama, LM Studio, llama.cpp's server, vLLM,
//! OpenRouter, api.openai.com. See [`client`] for the compatibility details that
//! make that true in practice rather than just on paper.

pub mod agent;
pub mod builder;
pub mod client;
pub mod describer;
pub mod instructions;
pub mod session_data;

pub use agent::{AgentProgress, Tool, ToolOutput};
pub use builder::{SkillBuilder, SkillTarget};
pub use client::{ConnectionTest, LlmClient};
pub use describer::Describer;
pub use session_data::SessionData;
