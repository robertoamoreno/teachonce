//! Shared vocabulary for Skill Recorder: the event schema every collector
//! produces, the on-disk session layout, the capture/LLM configuration, and the
//! deterministic timeline that turns a raw event stream into ordered steps.
//!
//! This crate deliberately has no platform or model dependencies — it is the
//! contract the capture, narration, agent, and recorder crates all agree on, and
//! it stays unit-testable without a screen, a microphone, or a network.

pub mod analysis;
pub mod clock;
pub mod config;
pub mod describe;
pub mod events;
pub mod frames;
pub mod narration;
pub mod paths;
pub mod session;
pub mod skill;
pub mod timeline;

pub use clock::epoch_ms;
pub use config::{CaptureConfig, LlmConfig, Settings};
pub use events::{EventInput, EventPayload, RecEvent};
pub use session::{SessionMeta, SessionStore};
