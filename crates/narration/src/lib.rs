//! On-device speech-to-text for narration, via whisper.cpp.
//!
//! Two things make this crate worth its compile time.
//!
//! **It is genuinely local.** The only network call in here downloads the model
//! weights once, when the user asks for it. Transcription itself never touches
//! the network, so narration — the most personal signal the recorder collects —
//! becomes text on the machine that recorded it, before analysis is even a
//! possibility.
//!
//! **It gets the clock right.** Whisper timestamps from the start of the *audio
//! file*. Narration can begin forty seconds into a recording, and there can be
//! several files if the user toggles the mic. Every timestamp is shifted onto
//! the session clock here, so a transcript line lands on the step it describes.

pub mod download;
pub mod transcribe;

pub use download::{ensure_model, is_model_cached, model_path, DownloadProgress};
pub use transcribe::{transcribe_session, Transcriber};
