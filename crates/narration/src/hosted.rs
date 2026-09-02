//! Hosted transcription: narration sent to an OpenAI-compatible
//! `/audio/transcriptions` endpoint instead of whisper.cpp.
//!
//! This is the one place narration audio can leave the machine, and it happens
//! only when the user has chosen a hosted endpoint in Settings and pressed
//! Transcribe. Everything else about narration stays as it is: the same WAVs,
//! the same session clock, the same hallucination filters.
//!
//! Two practical details shape the code. The recorder writes 32-bit float WAV
//! (about 64 KB per second), and hosted services cap uploads at roughly 25 MB,
//! so audio is re-encoded to 16-bit PCM and cut into five-minute chunks that
//! are stamped back onto the session clock individually. And the richer
//! `verbose_json` response, which carries per-segment timestamps, is not
//! supported by every model, so a rejection falls back to plain `json` and one
//! segment per chunk.

use std::io::Cursor;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use skillrec_core::config::{HostedTranscription, NarrationConfig};
use skillrec_core::narration::{is_meaningful_text, NarrationSegment, NarrationTranscript};
use skillrec_core::session::{read_json, write_json, SessionMeta};

use crate::transcribe::{read_wav, shift, AudioManifestRef, NO_SPEECH_THRESHOLD};

/// Whisper's native rate, and what the recorder writes.
const SAMPLE_RATE: u32 = 16_000;

/// Chunk length. Five minutes of 16-bit mono at 16 kHz is 9.6 MB, comfortably
/// under the 25 MB most services accept, with room for the multipart envelope.
pub const CHUNK_SECONDS: usize = 300;

/// Encode mono float samples as a 16-bit PCM WAV in memory.
pub fn encode_pcm16_wav(samples: &[f32]) -> Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = Cursor::new(Vec::with_capacity(44 + samples.len() * 2));
    {
        let mut writer = hound::WavWriter::new(&mut cursor, spec).context("starting the WAV")?;
        for sample in samples {
            let clamped = sample.clamp(-1.0, 1.0);
            writer
                .write_sample((clamped * i16::MAX as f32).round() as i16)
                .context("writing a sample")?;
        }
        writer.finalize().context("finalizing the WAV")?;
    }
    Ok(cursor.into_inner())
}

/// Split samples into upload-sized chunks.
pub fn chunks(samples: &[f32]) -> impl Iterator<Item = &[f32]> {
    samples.chunks(CHUNK_SECONDS * SAMPLE_RATE as usize)
}

/// Turn a transcription response into segments on the chunk's own clock.
///
/// `verbose_json` carries `segments` with `start`/`end` in seconds and, on
/// Whisper, a `no_speech_prob`; plain `json` carries only `text`, which becomes
/// one segment spanning the chunk. Silence hallucinations are dropped the same
/// way the local path drops them.
pub fn parse_response(body: &Value, chunk_ms: i64) -> Vec<NarrationSegment> {
    if let Some(segments) = body["segments"].as_array() {
        return segments
            .iter()
            .filter_map(|segment| {
                let text = segment["text"].as_str()?.trim().to_string();
                if segment["no_speech_prob"].as_f64().unwrap_or(0.0) > NO_SPEECH_THRESHOLD as f64 {
                    return None;
                }
                if !is_meaningful_text(&text) {
                    return None;
                }
                let start = segment["start"].as_f64().unwrap_or(0.0);
                let end = segment["end"].as_f64().unwrap_or(start);
                Some(NarrationSegment {
                    at_ms: (start * 1000.0).round() as i64,
                    end_ms: (end * 1000.0).round() as i64,
                    text,
                })
            })
            .collect();
    }
    let text = body["text"].as_str().unwrap_or("").trim();
    if !is_meaningful_text(text) {
        return Vec::new();
    }
    vec![NarrationSegment { at_ms: 0, end_ms: chunk_ms, text: text.to_string() }]
}

/// Uploads narration to one hosted endpoint.
pub struct HostedTranscriber {
    http: reqwest::Client,
    config: HostedTranscription,
    language: String,
}

impl HostedTranscriber {
    pub fn new(config: &HostedTranscription, language: &str) -> Result<Self> {
        config.validate()?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .build()
            .context("building the HTTP client")?;
        Ok(Self { http, config: config.clone(), language: language.trim().to_string() })
    }

    /// Transcribe one buffer, timestamps relative to the buffer's own start.
    pub async fn transcribe(
        &self,
        samples: &[f32],
        on_progress: &(dyn Fn(String) + Send + Sync),
    ) -> Result<Vec<NarrationSegment>> {
        let total = chunks(samples).count();
        let mut out = Vec::new();
        for (index, chunk) in chunks(samples).enumerate() {
            on_progress(if total > 1 {
                format!("Uploading narration part {} of {total}…", index + 1)
            } else {
                "Uploading narration…".into()
            });
            let offset_ms = (index * CHUNK_SECONDS * 1000) as i64;
            let chunk_ms = (chunk.len() as i64 * 1000) / SAMPLE_RATE as i64;
            let wav = encode_pcm16_wav(chunk)?;
            let segments = self.upload(wav, chunk_ms).await?;
            out.extend(shift(segments, offset_ms));
        }
        Ok(out)
    }

    async fn upload(&self, wav: Vec<u8>, chunk_ms: i64) -> Result<Vec<NarrationSegment>> {
        // Ask for timestamps first; fall back to plain text for models that
        // only speak `json`.
        match self.request(wav.clone(), "verbose_json").await {
            Ok(body) => Ok(parse_response(&body, chunk_ms)),
            Err(err) if is_format_rejection(&err) => {
                tracing::info!("the endpoint rejects verbose_json; retrying with json");
                let body = self.request(wav, "json").await?;
                Ok(parse_response(&body, chunk_ms))
            }
            Err(err) => Err(err),
        }
    }

    async fn request(&self, wav: Vec<u8>, response_format: &str) -> Result<Value> {
        let file = reqwest::multipart::Part::bytes(wav)
            .file_name("narration.wav")
            .mime_str("audio/wav")
            .context("describing the upload")?;
        let mut form = reqwest::multipart::Form::new()
            .text("model", self.config.model.clone())
            .text("response_format", response_format.to_string())
            .part("file", file);
        if !self.language.is_empty() && !self.language.eq_ignore_ascii_case("auto") {
            form = form.text("language", self.language.clone());
        }

        let mut request = self.http.post(self.config.transcriptions_url()).multipart(form);
        if !self.config.api_key.trim().is_empty() {
            request = request.bearer_auth(self.config.api_key.trim());
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("sending narration to {}", self.config.base_url))?;

        let status = response.status();
        let body = response.text().await.context("reading the transcription response")?;
        if !status.is_success() {
            let detail = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| body.chars().take(400).collect());
            anyhow::bail!("the transcription server answered {status}: {detail}");
        }
        serde_json::from_str(&body).context("parsing the transcription response")
    }
}

/// Did the server refuse `response_format=verbose_json` specifically?
fn is_format_rejection(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}").to_lowercase();
    text.contains("answered 4") && (text.contains("response_format") || text.contains("verbose_json"))
}

/// Transcribe every narration file in a recording through the hosted endpoint
/// and write `narration.json`, exactly as the local path does.
pub async fn transcribe_session_hosted(
    session_dir: &Path,
    config: &NarrationConfig,
    on_progress: &(dyn Fn(String) + Send + Sync),
) -> Result<NarrationTranscript> {
    let meta: SessionMeta = read_json(&session_dir.join("session.json"))
        .context("this recording has no readable session.json")?;
    let manifest: AudioManifestRef =
        read_json(&session_dir.join("audio.json")).unwrap_or_default();
    if manifest.segments.is_empty() {
        anyhow::bail!("this recording has no narration audio");
    }

    let transcriber = HostedTranscriber::new(&config.hosted, &config.language)?;
    let mut all = Vec::new();
    for segment in &manifest.segments {
        let path = skillrec_core::paths::resolve_within(session_dir, &segment.file)?;
        let samples = match read_wav(&path) {
            Ok(samples) => samples,
            Err(err) => {
                tracing::warn!(file = %segment.file, "skipping: {err:#}");
                continue;
            }
        };
        let offset = (segment.start_epoch - meta.started_at).max(0);
        tracing::info!(
            file = %segment.file,
            seconds = samples.len() / SAMPLE_RATE as usize,
            offset_ms = offset,
            endpoint = %config.hosted.base_url,
            "transcribing via hosted endpoint"
        );
        all.extend(shift(transcriber.transcribe(&samples, on_progress).await?, offset));
    }

    all.sort_by_key(|segment| segment.at_ms);
    let transcript = NarrationTranscript {
        model: format!("hosted:{}", config.hosted.model),
        language: config.language.clone(),
        segments: all,
    };
    write_json(&session_dir.join("narration.json"), &transcript)
        .context("saving narration.json")?;
    tracing::info!(segments = transcript.segments.len(), "narration transcribed via hosted endpoint");
    Ok(transcript)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    #[test]
    fn samples_round_trip_through_a_pcm16_wav() {
        let input = vec![0.0f32, 0.5, -0.5, 1.0, -1.0, 2.0];
        let wav = encode_pcm16_wav(&input).unwrap();
        assert!(wav.starts_with(b"RIFF"));
        let mut reader = hound::WavReader::new(Cursor::new(wav)).unwrap();
        let spec = reader.spec();
        assert_eq!((spec.channels, spec.sample_rate, spec.bits_per_sample), (1, 16_000, 16));
        let back: Vec<f32> =
            reader.samples::<i16>().map(|s| s.unwrap() as f32 / i16::MAX as f32).collect();
        assert_eq!(back.len(), 6);
        assert!((back[1] - 0.5).abs() < 1e-3);
        assert!((back[5] - 1.0).abs() < 1e-3, "out-of-range input is clamped, not wrapped");
    }

    #[test]
    fn long_narration_is_cut_into_upload_sized_chunks() {
        let per_chunk = CHUNK_SECONDS * SAMPLE_RATE as usize;
        let samples = vec![0.0f32; per_chunk * 2 + 5];
        let sizes: Vec<usize> = chunks(&samples).map(|c| c.len()).collect();
        assert_eq!(sizes, vec![per_chunk, per_chunk, 5]);
        // A chunk of 16-bit audio stays well under the 25 MB upload ceiling.
        assert!(per_chunk * 2 < 20_000_000);
        assert_eq!(chunks(&[]).count(), 0);
    }

    #[test]
    fn verbose_responses_become_timestamped_segments_minus_hallucinations() {
        let body = json!({
            "text": "Opening the invoice. Thanks for watching!",
            "segments": [
                { "start": 0.0, "end": 2.4, "text": " Opening the invoice.", "no_speech_prob": 0.02 },
                { "start": 2.4, "end": 4.0, "text": " Thanks for watching!", "no_speech_prob": 0.1 },
                { "start": 4.0, "end": 6.0, "text": " mumble", "no_speech_prob": 0.97 }
            ]
        });
        let segments = parse_response(&body, 6_000);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].at_ms, 0);
        assert_eq!(segments[0].end_ms, 2_400);
        assert_eq!(segments[0].text, "Opening the invoice.");
    }

    #[test]
    fn plain_responses_become_one_segment_spanning_the_chunk() {
        let segments = parse_response(&json!({ "text": " Now the export. " }), 12_000);
        assert_eq!(segments.len(), 1);
        assert_eq!((segments[0].at_ms, segments[0].end_ms), (0, 12_000));
        assert_eq!(segments[0].text, "Now the export.");
        assert!(parse_response(&json!({ "text": "[BLANK_AUDIO]" }), 1_000).is_empty());
    }

    #[test]
    fn only_a_format_complaint_triggers_the_json_fallback() {
        assert!(is_format_rejection(&anyhow::anyhow!(
            "the transcription server answered 400 Bad Request: response_format verbose_json is not supported"
        )));
        assert!(!is_format_rejection(&anyhow::anyhow!("the transcription server answered 401: bad key")));
        assert!(!is_format_rejection(&anyhow::anyhow!("the transcription server answered 500: verbose_json")));
    }

    /// A stub that answers one canned JSON body and records the raw request.
    fn stub(status: u16, body: Value) -> (String, Arc<Mutex<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(String::new()));
        let record = Arc::clone(&seen);
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap_or(0);
                }
                if line == "\r\n" {
                    break;
                }
            }
            let mut raw = vec![0u8; length];
            reader.read_exact(&mut raw).ok();
            *record.lock().unwrap() = String::from_utf8_lossy(&raw).into_owned();
            let payload = body.to_string();
            let reason = if status == 200 { "OK" } else { "Bad Request" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                payload.len(),
                payload
            );
            stream.write_all(response.as_bytes()).ok();
        });
        (format!("http://127.0.0.1:{port}/v1"), seen)
    }

    #[tokio::test]
    async fn the_upload_is_a_multipart_form_the_openai_api_understands() {
        let (base_url, seen) = stub(
            200,
            json!({ "text": "hello there", "segments": [{ "start": 0.5, "end": 1.5, "text": "hello there" }] }),
        );
        let config = HostedTranscription {
            base_url,
            api_key: "sk-test".into(),
            model: "whisper-1".into(),
            request_timeout_secs: 10,
        };
        let transcriber = HostedTranscriber::new(&config, "en").unwrap();
        let progress = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = {
            let progress = Arc::clone(&progress);
            move |message: String| progress.lock().unwrap().push(message)
        };

        let segments = transcriber.transcribe(&vec![0.1f32; 16_000], &sink).await.unwrap();
        assert_eq!(segments.len(), 1);
        assert_eq!((segments[0].at_ms, segments[0].end_ms), (500, 1_500));

        let raw = seen.lock().unwrap().clone();
        assert!(raw.contains("name=\"model\"\r\n\r\nwhisper-1"), "{raw:.300}");
        assert!(raw.contains("name=\"response_format\"\r\n\r\nverbose_json"));
        assert!(raw.contains("name=\"language\"\r\n\r\nen"));
        assert!(raw.contains("filename=\"narration.wav\""));
        assert!(raw.contains("RIFF"), "the WAV bytes ride in the form");
        assert_eq!(progress.lock().unwrap().as_slice(), ["Uploading narration…"]);
    }

    #[tokio::test]
    async fn a_second_chunk_is_shifted_by_the_chunk_length() {
        // Two chunks means two uploads; the stub only answers one, so run the
        // shift arithmetic directly instead of the network.
        let first = parse_response(&json!({ "text": "a", "segments": [{ "start": 1.0, "end": 2.0, "text": "part one" }] }), 300_000);
        let second = shift(
            parse_response(&json!({ "segments": [{ "start": 1.0, "end": 2.0, "text": "part two" }] }), 4_000),
            (CHUNK_SECONDS * 1000) as i64,
        );
        assert_eq!(first[0].at_ms, 1_000);
        assert_eq!(second[0].at_ms, 301_000);
    }
}
