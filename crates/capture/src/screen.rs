//! Screen stills.
//!
//! This is where the Rust port diverges most from the Electron original, and
//! deliberately. The original records a 1 fps VP8 WebM *and* writes deduplicated
//! JPEG snapshots alongside it, then needs ffmpeg or sharp to get frames back
//! out of the video. But nothing ever plays that video — the only consumer is a
//! model asking "what did the screen look like around this moment", which the
//! snapshots already answer.
//!
//! So there is no video here at all. The sampler grabs a still once a second,
//! throws away anything that looks like the previous one, and keeps the rest as
//! JPEGs. No encoder, no container, no decode step, no ffmpeg dependency — and a
//! ten-minute recording lands in the low tens of frames instead of 600.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use image::{DynamicImage, RgbaImage};
use skillrec_core::clock::{epoch_ms, to_at_ms};
use skillrec_core::events::EventPayload;
use skillrec_core::frames::{dhash, keep_reason, FrameManifest, FrameRecord};
use skillrec_core::session::write_json;

use crate::collector::{Collector, CollectorContext};

/// Sampling interval. Faster buys almost nothing: the compositor cost is per
/// grab, and screens do not meaningfully change more than once a second during
/// the kind of work this app records.
const SAMPLE: Duration = Duration::from_millis(1000);

/// Retained frames are downscaled to fit this box. A model reading a screenshot
/// needs to see layout and legible text, not pixels.
const MAX_WIDTH: u32 = 1280;
const MAX_HEIGHT: u32 = 720;

/// JPEG quality. 78 is the knee of the curve for UI screenshots — text stays
/// crisp, and files land around 60–150 KB.
const JPEG_QUALITY: u8 = 78;

/// Hard cap per recording, so a pathological session (a video playing on screen
/// for an hour) cannot fill the disk.
const MAX_FRAMES: usize = 600;

/// Fit `(width, height)` inside the box, preserving aspect ratio and never
/// upscaling.
pub fn fit_within(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (1, 1);
    }
    if width <= max_width && height <= max_height {
        return (width, height);
    }
    let scale = (max_width as f64 / width as f64).min(max_height as f64 / height as f64);
    ((width as f64 * scale).round().max(1.0) as u32, (height as f64 * scale).round().max(1.0) as u32)
}

/// The 9x8 grayscale thumbnail dHash is computed from.
///
/// 9 wide because a difference hash compares each pixel with its right-hand
/// neighbour, so eight comparisons per row need nine samples.
pub fn hash_thumbnail(image: &RgbaImage) -> Vec<u8> {
    let small = image::imageops::thumbnail(image, 9, 8);
    DynamicImage::ImageRgba8(small).to_luma8().into_raw()
}

/// Encode a downscaled JPEG.
pub fn encode_jpeg(image: &RgbaImage, max_width: u32, max_height: u32) -> Result<Vec<u8>> {
    let (width, height) = fit_within(image.width(), image.height(), max_width, max_height);
    let resized = image::imageops::thumbnail(image, width, height);
    let rgb = DynamicImage::ImageRgba8(resized).to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut Cursor::new(&mut out), JPEG_QUALITY)
        .encode_image(&rgb)
        .context("encoding the frame as JPEG")?;
    Ok(out)
}

/// Grab the primary display.
fn capture_primary() -> Result<RgbaImage> {
    let monitors = xcap::Monitor::all().context("listing displays")?;
    let monitor = monitors
        .iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .or_else(|| monitors.first())
        .context("no display is available to capture")?;
    monitor.capture_image().context("capturing the screen")
}

/// Samples the screen and keeps only the frames that carry new information.
pub struct ScreenCollector {
    sequence: usize,
    last_hash: Option<String>,
    last_kept_epoch: i64,
    frames: Vec<FrameRecord>,
    warned: bool,
    truncated: bool,
}

impl Default for ScreenCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl ScreenCollector {
    pub fn new() -> Self {
        Self {
            sequence: 0,
            last_hash: None,
            last_kept_epoch: 0,
            frames: Vec::new(),
            warned: false,
            truncated: false,
        }
    }

    /// Decide on one sample and, if it is worth keeping, write it.
    fn sample(&mut self, ctx: &CollectorContext, frames_dir: &Path) -> Result<()> {
        let image = capture_primary()?;
        let epoch = epoch_ms();
        let phash = dhash(&hash_thumbnail(&image));

        let since_last = if self.last_kept_epoch == 0 { 0 } else { epoch - self.last_kept_epoch };
        let Some(reason) = keep_reason(&phash, self.last_hash.as_deref(), since_last) else {
            return Ok(());
        };

        let bytes = encode_jpeg(&image, MAX_WIDTH, MAX_HEIGHT)?;
        self.sequence += 1;
        let name = format!("frame_{:06}.jpg", self.sequence);
        std::fs::write(frames_dir.join(&name), &bytes)
            .with_context(|| format!("writing frame {name}"))?;

        let (width, height) = fit_within(image.width(), image.height(), MAX_WIDTH, MAX_HEIGHT);
        let relative = format!("frames/{name}");
        self.last_hash = Some(phash.clone());
        self.last_kept_epoch = epoch;
        self.frames.push(FrameRecord {
            file: relative.clone(),
            at_ms: to_at_ms(epoch, ctx.started_at()),
            epoch,
            reason,
            phash: phash.clone(),
            width,
            height,
            bytes: bytes.len() as u64,
        });

        ctx.publish(
            "screen",
            EventPayload::FrameCaptured { file: relative, reason, phash, width, height },
        );
        Ok(())
    }
}

impl Collector for ScreenCollector {
    fn name(&self) -> &'static str {
        "screen"
    }

    fn run(&mut self, ctx: CollectorContext) {
        let frames_dir: PathBuf = ctx.session_dir().join("frames");
        if let Err(err) = std::fs::create_dir_all(&frames_dir) {
            tracing::warn!(%err, "cannot create the frames folder; skipping screen capture");
            return;
        }

        loop {
            if self.frames.len() >= MAX_FRAMES {
                // Stop sampling, but remember that we did: the manifest records
                // it so nothing downstream mistakes the frameless tail of a long
                // recording for a screen that simply never changed.
                self.truncated = true;
                tracing::warn!(
                    cap = MAX_FRAMES,
                    "frame cap reached; the rest of this recording will have no frames"
                );
                return;
            }
            if let Err(err) = self.sample(&ctx, &frames_dir) {
                // Warn once. A recording must not be abandoned because the
                // screen could not be grabbed — the event stream is the primary
                // signal and is still being collected.
                if !self.warned {
                    self.warned = true;
                    tracing::warn!(%err, "screen capture is unavailable; continuing without frames");
                }
            }
            if !ctx.sleep_or_stop(SAMPLE) {
                return;
            }
        }
    }

    /// Write `frames.json`, the index the describer's `list_frames` tool reads.
    fn finish(&mut self, ctx: &CollectorContext) {
        if self.frames.is_empty() {
            return;
        }
        let manifest =
            FrameManifest::with_truncation(std::mem::take(&mut self.frames), self.truncated);
        let path = ctx.session_dir().join("frames.json");
        if let Err(err) = write_json(&path, &manifest) {
            tracing::warn!(%err, "could not write the frame manifest");
        } else {
            tracing::info!(frames = manifest.frames.len(), "screen capture finished");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: u32, height: u32, shade: u8) -> RgbaImage {
        RgbaImage::from_fn(width, height, |x, _| {
            let value = ((x * 255 / width.max(1)) as u8).saturating_add(shade);
            image::Rgba([value, value, value, 255])
        })
    }

    #[test]
    fn oversized_screens_are_fitted_without_distortion() {
        // A 5K display keeps its 16:9 shape.
        assert_eq!(fit_within(5120, 2880, 1280, 720), (1280, 720));
        // An ultrawide is bounded by width.
        let (w, h) = fit_within(3440, 1440, 1280, 720);
        assert_eq!(w, 1280);
        assert!((h as f64 - 536.0).abs() < 2.0, "got {h}");
    }

    #[test]
    fn small_screens_are_never_upscaled() {
        assert_eq!(fit_within(800, 600, 1280, 720), (800, 600));
    }

    #[test]
    fn degenerate_sizes_do_not_divide_by_zero() {
        assert_eq!(fit_within(0, 0, 1280, 720), (1, 1));
        assert_eq!(fit_within(100, 0, 1280, 720), (1, 1));
    }

    #[test]
    fn the_hash_thumbnail_is_exactly_nine_by_eight() {
        assert_eq!(hash_thumbnail(&image(1920, 1080, 0)).len(), 9 * 8);
        assert_eq!(dhash(&hash_thumbnail(&image(1920, 1080, 0))).len(), 16);
    }

    #[test]
    fn identical_screens_hash_the_same_at_any_resolution() {
        // The same content on a Retina and a scaled display must dedupe against
        // each other, which is why hashing happens after the thumbnail.
        let a = dhash(&hash_thumbnail(&image(2560, 1440, 0)));
        let b = dhash(&hash_thumbnail(&image(1280, 720, 0)));
        assert_eq!(a, b);
    }

    #[test]
    fn encoding_produces_a_real_jpeg_within_the_box() {
        let bytes = encode_jpeg(&image(1920, 1080, 0), 1280, 720).unwrap();
        assert!(bytes.starts_with(&[0xFF, 0xD8]), "JPEG SOI marker");
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (1280, 720));
    }

    #[test]
    fn a_still_screen_is_kept_only_on_the_heartbeat() {
        use skillrec_core::events::FrameReason;
        use skillrec_core::frames::HEARTBEAT_MS;

        let hash = dhash(&hash_thumbnail(&image(1280, 720, 0)));
        assert_eq!(keep_reason(&hash, None, 0), Some(FrameReason::Initial));
        assert_eq!(keep_reason(&hash, Some(&hash), 1_000), None);
        assert_eq!(keep_reason(&hash, Some(&hash), HEARTBEAT_MS), Some(FrameReason::Heartbeat));
    }
}
