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

use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use image::{DynamicImage, RgbaImage};
use serde::Serialize;
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

/// One attached display, as the Settings picker lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplayInfo {
    /// The id macOS uses for it right now. It changes on every replug, which
    /// is why the settings store the name instead.
    pub id: u32,
    /// The name System Settings shows ("Built-in Retina Display",
    /// "DELL U2723QE"), numbered when two displays share one.
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub is_primary: bool,
}

/// The attached displays, primary first.
pub fn list_displays() -> Vec<DisplayInfo> {
    match xcap::Monitor::all() {
        Ok(monitors) => describe(&monitors).into_iter().map(|(info, _)| info).collect(),
        Err(err) => {
            tracing::warn!(%err, "could not list displays");
            Vec::new()
        }
    }
}

/// Pair each monitor with its description, primary first. The name lookup
/// goes through AppKit, so callers cache what this returns.
fn describe(monitors: &[xcap::Monitor]) -> Vec<(DisplayInfo, &xcap::Monitor)> {
    let raw: Vec<String> = monitors
        .iter()
        .map(|m| m.friendly_name().or_else(|_| m.name()).unwrap_or_else(|_| "Display".to_string()))
        .collect();
    let mut described: Vec<(DisplayInfo, &xcap::Monitor)> = monitors
        .iter()
        .zip(unique_names(&raw))
        .map(|(m, name)| {
            let info = DisplayInfo {
                id: m.id().unwrap_or(0),
                name,
                width: m.width().unwrap_or(0),
                height: m.height().unwrap_or(0),
                is_primary: m.is_primary().unwrap_or(false),
            };
            (info, m)
        })
        .collect();
    described.sort_by_key(|(info, _)| !info.is_primary);
    described
}

/// Two identical monitors report the same name. Number them so the setting
/// can tell them apart: "DELL U2723QE (1)", "DELL U2723QE (2)".
fn unique_names(raw: &[String]) -> Vec<String> {
    let mut seen: HashMap<&str, usize> = HashMap::new();
    raw.iter()
        .map(|name| {
            if raw.iter().filter(|other| *other == name).count() == 1 {
                return name.clone();
            }
            let n = seen.entry(name.as_str()).or_insert(0);
            *n += 1;
            format!("{name} ({n})")
        })
        .collect()
}

/// Which display to grab: the one named, else the primary, else the first.
/// The flag says the named display was missing and a stand-in was chosen.
fn choose(displays: &[DisplayInfo], wanted: &str) -> Option<(usize, bool)> {
    if !wanted.is_empty()
        && let Some(index) = displays.iter().position(|d| d.name == wanted)
    {
        return Some((index, false));
    }
    let fallback = displays.iter().position(|d| d.is_primary).or((!displays.is_empty()).then_some(0))?;
    Some((fallback, !wanted.is_empty()))
}

/// How many samples a stand-in display is used before the named one is looked
/// for again — so a replugged monitor is picked back up within seconds,
/// without an AppKit name lookup every second in between.
const RELOOK_EVERY: u32 = 10;

/// The display the stills come from: resolved lazily, remembered by id, and
/// resolved again when the monitor it landed on goes away.
struct DisplayTarget {
    /// The configured name; empty means the primary display.
    wanted: String,
    id: Option<u32>,
    standing_in: bool,
    samples_since_lookup: u32,
}

impl DisplayTarget {
    fn new(wanted: String) -> Self {
        Self { wanted, id: None, standing_in: false, samples_since_lookup: 0 }
    }

    /// Grab one frame from the right display.
    fn capture(&mut self) -> Result<RgbaImage> {
        let monitors = xcap::Monitor::all().context("listing displays")?;
        let monitor = self.pick(&monitors).context("no display is available to capture")?;
        monitor.capture_image().context("capturing the screen")
    }

    fn pick<'a>(&mut self, monitors: &'a [xcap::Monitor]) -> Option<&'a xcap::Monitor> {
        if self.wanted.is_empty() {
            // The default never asks AppKit for a name.
            return monitors
                .iter()
                .find(|m| m.is_primary().unwrap_or(false))
                .or_else(|| monitors.first());
        }
        self.samples_since_lookup += 1;
        let relook = self.standing_in && self.samples_since_lookup >= RELOOK_EVERY;
        if !relook
            && let Some(id) = self.id
            && let Some(monitor) = monitors.iter().find(|m| m.id().ok() == Some(id))
        {
            return Some(monitor);
        }
        self.samples_since_lookup = 0;
        let described = describe(monitors);
        let infos: Vec<DisplayInfo> = described.iter().map(|(info, _)| info.clone()).collect();
        let (index, standing_in) = choose(&infos, &self.wanted)?;
        let (info, monitor) = &described[index];
        if standing_in && !self.standing_in {
            tracing::warn!(
                wanted = %self.wanted,
                instead = %info.name,
                "the chosen display is not connected; recording the primary display instead"
            );
        } else if !standing_in && self.standing_in {
            tracing::info!(display = %self.wanted, "the chosen display is back");
        }
        self.id = Some(info.id);
        self.standing_in = standing_in;
        Some(monitor)
    }
}

/// Samples the screen and keeps only the frames that carry new information.
pub struct ScreenCollector {
    target: DisplayTarget,
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
    /// Record the primary display.
    pub fn new() -> Self {
        Self::for_display(String::new())
    }

    /// Record the display with this name (see [`DisplayInfo::name`]); empty
    /// means the primary display. A name that is not connected when sampling
    /// starts falls back to the primary display, with a warning.
    pub fn for_display(name: impl Into<String>) -> Self {
        Self {
            target: DisplayTarget::new(name.into()),
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
        let image = self.target.capture()?;
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

    fn display(name: &str, is_primary: bool) -> DisplayInfo {
        DisplayInfo { id: 1, name: name.to_string(), width: 1920, height: 1080, is_primary }
    }

    #[test]
    fn identical_monitors_get_numbered_names() {
        let raw: Vec<String> =
            ["Built-in Retina Display", "DELL U2723QE", "DELL U2723QE"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            unique_names(&raw),
            vec!["Built-in Retina Display", "DELL U2723QE (1)", "DELL U2723QE (2)"]
        );
    }

    #[test]
    fn the_named_display_is_chosen_and_the_primary_stands_in_when_it_is_missing() {
        let displays = vec![display("Built-in Retina Display", true), display("DELL U2723QE", false)];
        // No choice: the primary, and nothing to warn about.
        assert_eq!(choose(&displays, ""), Some((0, false)));
        // A choice that is connected.
        assert_eq!(choose(&displays, "DELL U2723QE"), Some((1, false)));
        // A choice that is not: the primary, flagged so the collector can say so.
        assert_eq!(choose(&displays, "LG HDR 4K"), Some((0, true)));
        // No primary at all (a headless Mac): the first display.
        let secondary_only = vec![display("DELL U2723QE", false)];
        assert_eq!(choose(&secondary_only, "LG HDR 4K"), Some((0, true)));
        assert_eq!(choose(&[], "LG HDR 4K"), None);
    }

    /// On a real Mac: `cargo test -p skillrec-capture -- --ignored displays`.
    #[test]
    #[ignore = "needs a display to enumerate"]
    fn the_attached_displays_are_listed_primary_first() {
        let displays = list_displays();
        for d in &displays {
            eprintln!("{} {}x{} primary={} id={}", d.name, d.width, d.height, d.is_primary, d.id);
        }
        assert!(!displays.is_empty());
        assert!(displays[0].is_primary);
        let names: std::collections::HashSet<&str> = displays.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names.len(), displays.len(), "names must be unique");
    }

    /// On a real Mac with Screen Recording granted to the test binary:
    /// `cargo test -p skillrec-capture -- --ignored captured`.
    #[test]
    #[ignore = "needs a display and Screen Recording"]
    fn the_named_display_is_the_one_captured_and_a_missing_one_falls_back() {
        let aspect = |w: u32, h: u32| w as f64 / h as f64;
        let displays = list_displays();
        let primary = &displays[0];
        // A display that is not the primary, when there is one.
        if let Some(other) = displays.iter().find(|d| !d.is_primary) {
            let image = DisplayTarget::new(other.name.clone()).capture().unwrap();
            assert!(
                (aspect(image.width(), image.height()) - aspect(other.width, other.height)).abs() < 0.02,
                "{}x{} is not the shape of {}", image.width(), image.height(), other.name
            );
        }
        // A display that is not connected: the primary stands in.
        let mut target = DisplayTarget::new("No Such Display".to_string());
        let image = target.capture().unwrap();
        assert!(target.standing_in);
        assert!((aspect(image.width(), image.height()) - aspect(primary.width, primary.height)).abs() < 0.02);
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
