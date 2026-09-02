//! Turning narration WAVs into a transcript on the session clock.

use std::path::Path;

use anyhow::{Context, Result};
use skillrec_core::config::{NarrationConfig, WhisperModel};
use skillrec_core::narration::{is_meaningful_text, NarrationSegment, NarrationTranscript};
use skillrec_core::session::{read_json, write_json, SessionMeta};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

/// One narration file, with the wall-clock time it started.
///
/// Mirrors `skillrec_capture::audio::AudioSegment` without depending on that
/// crate — transcription should not pull in the whole capture stack.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioSegmentRef {
    pub file: String,
    pub start_epoch: i64,
    #[serde(default)]
    pub stop_epoch: i64,
    #[serde(default)]
    pub sample_rate: u32,
    #[serde(default)]
    pub samples: usize,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioManifestRef {
    #[serde(default)]
    pub segments: Vec<AudioSegmentRef>,
}

/// Read a 16 kHz mono float WAV into samples.
///
/// The recorder always writes exactly this format, but a file can still be
/// wrong — hand-copied in, or from an older build — so the rate is checked
/// rather than assumed. Feeding Whisper 48 kHz audio it believes is 16 kHz
/// produces fluent, confident, entirely wrong transcripts.
pub fn read_wav(path: &Path) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let spec = reader.spec();
    anyhow::ensure!(
        spec.sample_rate == 16_000,
        "{} is {} Hz; Whisper needs 16 kHz",
        path.display(),
        spec.sample_rate
    );

    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().filter_map(Result::ok).collect(),
        hound::SampleFormat::Int => reader
            .samples::<i32>()
            .filter_map(Result::ok)
            .map(|s| s as f32 / i32::MAX as f32)
            .collect(),
    };

    if spec.channels <= 1 {
        return Ok(samples);
    }
    let channels = spec.channels as usize;
    Ok(samples
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
        .collect())
}

/// Above this, Whisper itself believes the span is not speech. Set loosely: a
/// quiet but real aside should survive, and the boilerplate filter is the second
/// line of defence. Shared with the hosted path, which gets the same figure
/// back from the API.
pub(crate) const NO_SPEECH_THRESHOLD: f32 = 0.9;

/// A loaded Whisper model, reusable across segments.
pub struct Transcriber {
    context: WhisperContext,
    model: WhisperModel,
    language: String,
}

impl Transcriber {
    /// Load the weights. Expensive — hundreds of milliseconds to seconds — so
    /// one instance is reused for every segment of a session.
    pub fn load(model: WhisperModel, language: &str, weights: &Path) -> Result<Self> {
        let context = WhisperContext::new_with_params(weights, WhisperContextParameters::default())
            .with_context(|| format!("loading {}", weights.display()))?;
        Ok(Self { context, model, language: language.to_string() })
    }

    /// Transcribe one buffer, timestamps relative to the buffer's own start.
    pub fn transcribe(&self, samples: &[f32]) -> Result<Vec<NarrationSegment>> {
        if samples.is_empty() {
            return Ok(Vec::new());
        }

        // Greedy sampling: beam search costs several times more for transcript
        // differences that do not survive the hallucination filter anyway.
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_translate(false);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        // "auto" means Whisper detects the language itself, which is what we
        // want by default — the recorder should not assume everyone works in
        // English.
        if self.language != "auto" {
            params.set_language(Some(&self.language));
        }
        params.set_n_threads(recommended_threads());

        let mut state = self.context.create_state().context("creating a Whisper state")?;
        state.full(params, samples).context("running Whisper")?;

        let mut out = Vec::new();
        for index in 0..state.full_n_segments() {
            let Some(segment) = state.get_segment(index) else {
                continue;
            };
            let Ok(text) = segment.to_str_lossy() else {
                continue;
            };
            let text = text.trim().to_string();

            // Two filters, because Whisper is confidently wrong about silence.
            // The model's own no-speech probability catches most of it; the
            // boilerplate list catches the rest ("Thanks for watching!"), which
            // it often reports as perfectly confident speech.
            if segment.no_speech_probability() > NO_SPEECH_THRESHOLD {
                continue;
            }
            if !is_meaningful_text(&text) {
                continue;
            }

            // whisper.cpp reports centiseconds.
            out.push(NarrationSegment {
                at_ms: segment.start_timestamp() * 10,
                end_ms: segment.end_timestamp() * 10,
                text,
            });
        }
        Ok(out)
    }

    pub fn model_id(&self) -> String {
        self.model.file_name().trim_end_matches(".bin").to_string()
    }
}

/// Leave headroom so transcription does not make the machine unusable — this
/// runs while the user is still working.
fn recommended_threads() -> std::ffi::c_int {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    cores.saturating_sub(2).clamp(1, 8) as std::ffi::c_int
}

/// Shift a segment's timestamps onto the session clock.
pub fn shift(segments: Vec<NarrationSegment>, offset_ms: i64) -> Vec<NarrationSegment> {
    segments
        .into_iter()
        .map(|segment| NarrationSegment {
            at_ms: (segment.at_ms + offset_ms).max(0),
            end_ms: (segment.end_ms + offset_ms).max(0),
            text: segment.text,
        })
        .collect()
}

/// Transcribe every narration file in a recording and write `narration.json`.
pub fn transcribe_session(
    session_dir: &Path,
    config: &NarrationConfig,
    weights: &Path,
) -> Result<NarrationTranscript> {
    let meta: SessionMeta = read_json(&session_dir.join("session.json"))
        .context("this recording has no readable session.json")?;
    let manifest: AudioManifestRef =
        read_json(&session_dir.join("audio.json")).unwrap_or_default();

    if manifest.segments.is_empty() {
        anyhow::bail!("this recording has no narration audio");
    }

    let transcriber = Transcriber::load(config.model, &config.language, weights)?;
    let mut all = Vec::new();

    for segment in &manifest.segments {
        let path = skillrec_core::paths::resolve_within(session_dir, &segment.file)?;
        let samples = match read_wav(&path) {
            Ok(samples) => samples,
            Err(err) => {
                // One unreadable file must not lose the rest of the narration.
                tracing::warn!(file = %segment.file, "skipping: {err:#}");
                continue;
            }
        };
        let offset = (segment.start_epoch - meta.started_at).max(0);
        tracing::info!(
            file = %segment.file,
            seconds = samples.len() / 16_000,
            offset_ms = offset,
            "transcribing"
        );
        all.extend(shift(transcriber.transcribe(&samples)?, offset));
    }

    all.sort_by_key(|segment| segment.at_ms);
    let transcript = NarrationTranscript {
        model: transcriber.model_id(),
        language: config.language.clone(),
        segments: all,
    };
    write_json(&session_dir.join("narration.json"), &transcript)
        .context("saving narration.json")?;
    tracing::info!(segments = transcript.segments.len(), "narration transcribed");
    Ok(transcript)
}

/// Must a recording be transcribed before it can be analysed?
///
/// Narration is the user's own statement of intent, so analysis must never
/// silently run without it when it exists but has not been transcribed. A
/// recording with no audio, or one already transcribed, analyses straight away.
pub fn needs_transcription(session_dir: &Path) -> bool {
    let manifest: AudioManifestRef =
        read_json(&session_dir.join("audio.json")).unwrap_or_default();
    !manifest.segments.is_empty() && !session_dir.join("narration.json").exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_wav(path: &Path, rate: u32, samples: &[f32]) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for sample in samples {
            writer.write_sample(*sample).unwrap();
        }
        writer.finalize().unwrap();
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("skillrec-narr-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_16k_mono_wav_round_trips() {
        let dir = temp_dir("wav");
        let path = dir.join("a.wav");
        write_wav(&path, 16_000, &[0.0, 0.5, -0.5, 0.25]);
        let samples = read_wav(&path).unwrap();
        assert_eq!(samples.len(), 4);
        assert!((samples[1] - 0.5).abs() < 1e-6);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_wrong_sample_rate_is_refused_rather_than_silently_mistranscribed() {
        // Whisper given 48 kHz audio it thinks is 16 kHz produces fluent
        // nonsense — far worse than an error.
        let dir = temp_dir("rate");
        let path = dir.join("a.wav");
        write_wav(&path, 48_000, &[0.0; 16]);
        let err = read_wav(&path).unwrap_err().to_string();
        assert!(err.contains("48000"), "{err}");
        assert!(err.contains("16 kHz"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn timestamps_shift_onto_the_session_clock() {
        // Narration that began 40s into the recording must land 40s in.
        let segments = vec![
            NarrationSegment { at_ms: 0, end_ms: 1_500, text: "first".into() },
            NarrationSegment { at_ms: 2_000, end_ms: 3_000, text: "second".into() },
        ];
        let shifted = shift(segments, 40_000);
        assert_eq!(shifted[0].at_ms, 40_000);
        assert_eq!(shifted[1].end_ms, 43_000);
    }

    #[test]
    fn shifting_never_produces_a_negative_timestamp() {
        let segments = vec![NarrationSegment { at_ms: 100, end_ms: 200, text: "x".into() }];
        assert_eq!(shift(segments, -5_000)[0].at_ms, 0);
    }

    #[test]
    fn a_recording_with_no_audio_needs_no_transcription() {
        let dir = temp_dir("gate");
        assert!(!needs_transcription(&dir), "no audio means nothing to transcribe");

        write_json(
            &dir.join("audio.json"),
            &AudioManifestRef {
                segments: vec![AudioSegmentRef {
                    file: "audio/a.wav".into(),
                    start_epoch: 1_000,
                    stop_epoch: 2_000,
                    sample_rate: 16_000,
                    samples: 16_000,
                }],
            },
        )
        .unwrap();
        assert!(needs_transcription(&dir), "audio with no transcript must gate analysis");

        write_json(&dir.join("narration.json"), &NarrationTranscript::default()).unwrap();
        assert!(!needs_transcription(&dir), "an existing transcript releases the gate");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn thread_count_leaves_the_machine_usable() {
        let threads = recommended_threads();
        assert!(threads >= 1);
        assert!(threads <= 8, "must not saturate every core while the user works");
    }
}
