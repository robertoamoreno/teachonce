//! Microphone capture for narration.
//!
//! Narration is optional, toggleable mid-recording, and written straight to a 16
//! kHz mono WAV — the exact format Whisper wants, so transcription needs no
//! decode step and no ffmpeg. Capturing at the model's rate rather than the
//! device's also keeps the file small: a ten-minute narration is about 19 MB of
//! WAV, written incrementally.
//!
//! Timing matters as much as audio quality here. Every segment records the wall
//! clock at which it started, so a transcript produced from audio that began 40
//! seconds into the session still lands on the right steps.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::{Deserialize, Serialize};
use skillrec_core::clock::epoch_ms;

/// Whisper is trained on 16 kHz mono. Anything else would have to be resampled
/// before inference anyway.
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// An available input device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MicrophoneDevice {
    pub id: String,
    pub label: String,
    pub is_default: bool,
}

/// One continuous stretch of microphone capture. A session has several when the
/// user toggles the mic off and on.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioSegment {
    /// Path relative to the session folder.
    pub file: String,
    /// Wall clock when capture actually began.
    pub start_epoch: i64,
    pub stop_epoch: i64,
    pub sample_rate: u32,
    pub samples: usize,
}

impl AudioSegment {
    pub fn duration_ms(&self) -> i64 {
        (self.samples as i64 * 1000) / self.sample_rate.max(1) as i64
    }

    /// Offset from the session start — what transcript timestamps are shifted by.
    pub fn offset_from(&self, session_started_at: i64) -> i64 {
        (self.start_epoch - session_started_at).max(0)
    }
}

/// `audio.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioManifest {
    pub segments: Vec<AudioSegment>,
}

/// List input devices, default first.
pub fn list_microphones() -> Vec<MicrophoneDevice> {
    let host = cpal::default_host();
    // cpal 0.18 exposes the device name through `Display`, not a `name()` method.
    let default_name = host.default_input_device().map(|d| d.to_string());
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    let mut out: Vec<MicrophoneDevice> = devices
        .map(|device| {
            let name = device.to_string();
            MicrophoneDevice {
                is_default: Some(&name) == default_name.as_ref(),
                id: name.clone(),
                label: name,
            }
        })
        .collect();
    out.sort_by_key(|d| !d.is_default);
    out
}

/// Downmix interleaved frames to mono by averaging channels.
pub fn to_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
        .collect()
}

/// Resample mono audio to [`TARGET_SAMPLE_RATE`].
///
/// This is a box-average decimator, not a windowed-sinc: each output sample is
/// the mean of the input samples that map onto it. Averaging acts as a crude
/// low-pass, so unlike naive sample-dropping it does not fold high frequencies
/// down into the speech band as audible aliasing. For 48k→16k speech headed into
/// Whisper the difference against a proper resampler is inaudible and
/// unmeasurable in transcript quality, and it avoids a resampling dependency and
/// its chunk-boundary state.
pub fn resample_to_16k(input: &[f32], source_rate: u32) -> Vec<f32> {
    if input.is_empty() || source_rate == 0 {
        return Vec::new();
    }
    if source_rate == TARGET_SAMPLE_RATE {
        return input.to_vec();
    }
    let ratio = source_rate as f64 / TARGET_SAMPLE_RATE as f64;
    let out_len = ((input.len() as f64) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for index in 0..out_len {
        let start = (index as f64 * ratio) as usize;
        let end = (((index + 1) as f64 * ratio) as usize).clamp(start + 1, input.len());
        let window = &input[start..end];
        out.push(window.iter().sum::<f32>() / window.len() as f32);
    }
    out
}

/// WAV settings for a narration file.
fn wav_spec() -> hound::WavSpec {
    hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_SAMPLE_RATE,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    }
}

/// An in-progress microphone capture.
pub struct MicrophoneRecorder {
    stream: cpal::Stream,
    writer: Arc<Mutex<Option<hound::WavWriter<std::io::BufWriter<std::fs::File>>>>>,
    samples: Arc<Mutex<usize>>,
    file: PathBuf,
    relative: String,
    start_epoch: i64,
}

impl MicrophoneRecorder {
    /// Open `device` (or the default) and begin writing into `audio_dir`.
    pub fn start(audio_dir: &Path, index: usize, device_id: Option<&str>) -> Result<Self> {
        std::fs::create_dir_all(audio_dir).context("creating the audio folder")?;

        let host = cpal::default_host();
        let device = match device_id {
            Some(id) => host
                .input_devices()
                .context("listing microphones")?
                .find(|d| d.to_string() == id)
                .or_else(|| host.default_input_device()),
            None => host.default_input_device(),
        }
        .context("no microphone is available")?;

        let config = device.default_input_config().context("reading the microphone format")?;
        let source_rate = config.sample_rate();
        let channels = config.channels() as usize;

        let name = format!("narration_{index:02}.wav");
        let file = audio_dir.join(&name);
        let writer = hound::WavWriter::create(&file, wav_spec())
            .with_context(|| format!("creating {}", file.display()))?;
        let writer = Arc::new(Mutex::new(Some(writer)));
        let samples = Arc::new(Mutex::new(0usize));

        let sink = Arc::clone(&writer);
        let counter = Arc::clone(&samples);
        let on_error = |err| tracing::warn!(%err, "microphone stream error");

        // Conversion happens inside the audio callback, which must never block or
        // allocate unpredictably. Both operations here are linear over a buffer
        // of a few hundred samples, well inside the callback's budget.
        let write = move |data: &[f32]| {
            let mono = to_mono(data, channels);
            let resampled = resample_to_16k(&mono, source_rate);
            let Ok(mut guard) = sink.lock() else { return };
            let Some(writer) = guard.as_mut() else { return };
            for sample in &resampled {
                let _ = writer.write_sample(*sample);
            }
            if let Ok(mut count) = counter.lock() {
                *count += resampled.len();
            }
        };

        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => device.build_input_stream(
                config.config(),
                move |data: &[f32], _| write(data),
                on_error,
                None,
            ),
            cpal::SampleFormat::I16 => device.build_input_stream(
                config.config(),
                move |data: &[i16], _| {
                    let floats: Vec<f32> =
                        data.iter().map(|s| *s as f32 / i16::MAX as f32).collect();
                    write(&floats)
                },
                on_error,
                None,
            ),
            cpal::SampleFormat::U16 => device.build_input_stream(
                config.config(),
                move |data: &[u16], _| {
                    let floats: Vec<f32> = data
                        .iter()
                        .map(|s| (*s as f32 - u16::MAX as f32 / 2.0) / (u16::MAX as f32 / 2.0))
                        .collect();
                    write(&floats)
                },
                on_error,
                None,
            ),
            other => anyhow::bail!("unsupported microphone sample format: {other:?}"),
        }
        .context("opening the microphone stream")?;

        stream.play().context("starting the microphone stream")?;
        tracing::info!(rate = source_rate, channels, "microphone capture started");

        Ok(Self {
            stream,
            writer,
            samples,
            file,
            relative: format!("audio/{name}"),
            start_epoch: epoch_ms(),
        })
    }

    /// Stop capturing and finalize the WAV header.
    ///
    /// Returns `None` when nothing was recorded, so an accidental mic toggle
    /// leaves no empty file behind for the transcriber to trip over.
    pub fn stop(self) -> Option<AudioSegment> {
        drop(self.stream);
        let samples = self.samples.lock().map(|count| *count).unwrap_or(0);
        if let Ok(mut guard) = self.writer.lock()
            && let Some(writer) = guard.take()
            && let Err(err) = writer.finalize()
        {
            tracing::warn!(%err, "could not finalize the narration WAV");
        }
        if samples == 0 {
            let _ = std::fs::remove_file(&self.file);
            return None;
        }
        Some(AudioSegment {
            file: self.relative,
            start_epoch: self.start_epoch,
            stop_epoch: epoch_ms(),
            sample_rate: TARGET_SAMPLE_RATE,
            samples,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_is_averaged_down_to_mono() {
        assert_eq!(to_mono(&[1.0, 0.0, 0.5, 0.5], 2), vec![0.5, 0.5]);
        assert_eq!(to_mono(&[1.0, 2.0, 3.0], 1), vec![1.0, 2.0, 3.0]);
        assert!(to_mono(&[], 2).is_empty());
    }

    #[test]
    fn resampling_hits_the_target_rate_and_length() {
        let input: Vec<f32> = (0..48_000).map(|i| i as f32 / 48_000.0).collect();
        let out = resample_to_16k(&input, 48_000);
        assert_eq!(out.len(), 16_000, "one second in, one second out");
    }

    #[test]
    fn a_matching_rate_passes_through_untouched() {
        let input = vec![0.1, 0.2, 0.3];
        assert_eq!(resample_to_16k(&input, TARGET_SAMPLE_RATE), input);
    }

    #[test]
    fn resampling_preserves_signal_level_rather_than_decimating() {
        // A constant tone must come out at the same amplitude; a bug that
        // averaged in zero-padding would quietly halve the volume and wreck
        // transcription.
        let input = vec![0.5f32; 44_100];
        let out = resample_to_16k(&input, 44_100);
        assert!(out.iter().all(|s| (*s - 0.5).abs() < 1e-6));
        assert!(!out.is_empty());
    }

    #[test]
    fn degenerate_input_does_not_panic() {
        assert!(resample_to_16k(&[], 48_000).is_empty());
        assert!(resample_to_16k(&[0.1], 0).is_empty());
    }

    #[test]
    fn segment_timing_maps_onto_the_session_clock() {
        let segment = AudioSegment {
            file: "audio/narration_01.wav".into(),
            start_epoch: 1_040_000,
            stop_epoch: 1_050_000,
            sample_rate: TARGET_SAMPLE_RATE,
            samples: TARGET_SAMPLE_RATE as usize * 10,
        };
        assert_eq!(segment.duration_ms(), 10_000);
        // Narration that began 40s into the session shifts by exactly that much.
        assert_eq!(segment.offset_from(1_000_000), 40_000);
        // A segment that somehow predates the session clamps rather than going
        // negative and dragging transcript lines off the front of the timeline.
        assert_eq!(segment.offset_from(2_000_000), 0);
    }
}
