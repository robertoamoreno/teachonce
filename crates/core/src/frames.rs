//! Screen stills: the perceptual hash that decides which ones are worth keeping,
//! and the manifest describing the ones that were.
//!
//! The whole point of this module is that **we never store video**. Screen
//! capture's real cost is the OS compositor handing us the framebuffer, and a
//! recording is only ever read by an LLM looking for "what did the screen look
//! like around this event". So the recorder samples a still roughly once a
//! second, throws away everything that looks like the previous one, and keeps a
//! handful of JPEGs. A 10-minute session typically lands in the low tens of
//! frames instead of a 600-frame video that then needs decoding.

use serde::{Deserialize, Serialize};

use crate::clock::{AtMs, EpochMs};
use crate::events::FrameReason;

/// Rows/columns of the difference hash. 8x8 bits = a 64-bit hash rendered as 16
/// hex characters.
const HASH_SIZE: usize = 8;

/// Two frames whose hashes differ by at most this many bits are "the same
/// screen". Tuned like the upstream app: high enough to ignore a blinking
/// cursor, a clock tick, or JPEG noise; low enough to notice a dialog opening.
pub const DEDUPE_THRESHOLD: u32 = 8;

/// How long the screen may stay unchanged before a still is kept anyway. Without
/// a heartbeat, a user reading a long page for a minute produces no visual
/// evidence at all for that stretch.
pub const HEARTBEAT_MS: i64 = 5_000;

/// One retained still.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameRecord {
    /// Path relative to the session folder, e.g. `frames/frame_000012.jpg`.
    pub file: String,
    /// Milliseconds since the recording started.
    pub at_ms: AtMs,
    /// Wall clock, for correlating against collectors that stamp in epoch time.
    pub epoch: EpochMs,
    /// Why this one was kept.
    pub reason: FrameReason,
    /// 16 hex characters of difference hash.
    pub phash: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
}

/// `frames.json` — the index the describer's `list_frames` tool reads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameManifest {
    pub version: u32,
    pub heartbeat_ms: i64,
    /// True when capture stopped at the per-recording cap before the recording
    /// ended, so frames cover only the start of the session.
    ///
    /// Recorded rather than merely logged because the absence of frames after
    /// some point is otherwise indistinguishable from "the screen never
    /// changed" — and a describer reading it that way would draw exactly the
    /// wrong conclusion about the tail of a long recording.
    #[serde(default)]
    pub truncated: bool,
    pub frames: Vec<FrameRecord>,
}

pub const FRAME_MANIFEST_VERSION: u32 = 1;

impl FrameManifest {
    pub fn new(frames: Vec<FrameRecord>) -> Self {
        Self::with_truncation(frames, false)
    }

    pub fn with_truncation(mut frames: Vec<FrameRecord>, truncated: bool) -> Self {
        frames.sort_by_key(|f| f.at_ms);
        Self {
            version: FRAME_MANIFEST_VERSION,
            heartbeat_ms: HEARTBEAT_MS,
            truncated,
            frames,
        }
    }

    /// The last moment covered by a frame, or `None` when there are none.
    pub fn covers_to_ms(&self) -> Option<AtMs> {
        self.frames.last().map(|f| f.at_ms)
    }

    /// The frame nearest a given moment, for "show me what the screen looked
    /// like when this event fired".
    pub fn nearest(&self, at_ms: AtMs) -> Option<&FrameRecord> {
        self.frames
            .iter()
            .min_by_key(|f| (f.at_ms - at_ms).abs())
    }

    /// Every frame inside a window, capped and evenly thinned when the window is
    /// dense — the describer asks for a time range, not a frame count, and must
    /// not be handed 200 images.
    pub fn window(&self, from_ms: AtMs, to_ms: AtMs, max: usize) -> Vec<&FrameRecord> {
        let hits: Vec<&FrameRecord> = self
            .frames
            .iter()
            .filter(|f| f.at_ms >= from_ms && f.at_ms <= to_ms)
            .collect();
        thin(hits, max)
    }
}

/// Evenly sample at most `max` items, always keeping the first and last.
fn thin<T>(items: Vec<T>, max: usize) -> Vec<T> {
    if max == 0 {
        return Vec::new();
    }
    if items.len() <= max {
        return items;
    }
    if max == 1 {
        return items.into_iter().take(1).collect();
    }
    // Pick the index closest to each of `max` evenly spaced positions across the
    // range, which pins slot 0 to the first item and slot max-1 to the last.
    let last = (items.len() - 1) as f64;
    let wanted: Vec<usize> = (0..max)
        .map(|slot| (slot as f64 * last / (max - 1) as f64).round() as usize)
        .collect();
    items
        .into_iter()
        .enumerate()
        .filter(|(index, _)| wanted.contains(index))
        .map(|(_, item)| item)
        .collect()
}

/// Difference hash of a `(HASH_SIZE+1) x HASH_SIZE` grayscale thumbnail.
///
/// dHash compares each pixel with its right-hand neighbour, so it encodes
/// *structure* (where the edges are) rather than brightness. That makes it
/// immune to the gradual backlight and colour-profile drift that would make a
/// plain checksum consider every single frame different.
///
/// `luma` must be row-major, `HASH_SIZE + 1` wide and `HASH_SIZE` tall.
pub fn dhash(luma: &[u8]) -> String {
    let width = HASH_SIZE + 1;
    debug_assert_eq!(luma.len(), width * HASH_SIZE, "dhash expects a 9x8 thumbnail");
    if luma.len() < width * HASH_SIZE {
        return String::new();
    }
    let mut bits = 0u64;
    let mut index = 0;
    for row in 0..HASH_SIZE {
        for col in 0..HASH_SIZE {
            let left = luma[row * width + col];
            let right = luma[row * width + col + 1];
            if left < right {
                bits |= 1 << index;
            }
            index += 1;
        }
    }
    format!("{bits:016x}")
}

/// Bit distance between two hex hashes. Mismatched or malformed input is treated
/// as maximally different, so a decoding hiccup keeps a frame rather than
/// silently dropping it.
pub fn hamming(a: &str, b: &str) -> u32 {
    if a.len() != b.len() || a.is_empty() {
        return u32::MAX;
    }
    match (u64::from_str_radix(a, 16), u64::from_str_radix(b, 16)) {
        (Ok(x), Ok(y)) => (x ^ y).count_ones(),
        _ => u32::MAX,
    }
}

/// Should this frame be kept?
///
/// Returns the reason to keep it, or `None` to discard.
pub fn keep_reason(
    phash: &str,
    previous: Option<&str>,
    since_last_kept_ms: i64,
) -> Option<FrameReason> {
    let Some(previous) = previous else {
        return Some(FrameReason::Initial);
    };
    if hamming(phash, previous) > DEDUPE_THRESHOLD {
        return Some(FrameReason::Changed);
    }
    if since_last_kept_ms >= HEARTBEAT_MS {
        return Some(FrameReason::Heartbeat);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 9x8 gradient: each pixel is brighter than the one to its left.
    fn gradient() -> Vec<u8> {
        (0..9 * 8).map(|i| ((i % 9) * 28) as u8).collect()
    }

    #[test]
    fn identical_screens_hash_identically() {
        assert_eq!(dhash(&gradient()), dhash(&gradient()));
        assert_eq!(hamming(&dhash(&gradient()), &dhash(&gradient())), 0);
    }

    #[test]
    fn a_uniform_screen_and_a_gradient_are_far_apart() {
        let flat = vec![128u8; 9 * 8];
        assert!(hamming(&dhash(&flat), &dhash(&gradient())) > DEDUPE_THRESHOLD);
    }

    #[test]
    fn overall_brightness_shifts_do_not_change_the_hash() {
        // Structure is identical, every pixel is 20 brighter: a backlight or
        // colour-profile change must not read as "the screen changed".
        let dimmer: Vec<u8> = gradient().iter().map(|p| p.saturating_sub(20)).collect();
        assert_eq!(dhash(&gradient()), dhash(&dimmer));
    }

    #[test]
    fn frames_are_kept_on_change_or_heartbeat_only() {
        let a = dhash(&gradient());
        let flat = dhash(&[128u8; 9 * 8]);

        assert_eq!(keep_reason(&a, None, 0), Some(FrameReason::Initial));
        assert_eq!(keep_reason(&flat, Some(&a), 100), Some(FrameReason::Changed));
        assert_eq!(keep_reason(&a, Some(&a), 100), None);
        assert_eq!(keep_reason(&a, Some(&a), HEARTBEAT_MS), Some(FrameReason::Heartbeat));
    }

    #[test]
    fn malformed_hashes_compare_as_maximally_different() {
        // Fail open: a frame we cannot compare is kept, never silently dropped.
        assert_eq!(hamming("zzzz", "0000"), u32::MAX);
        assert_eq!(hamming("", ""), u32::MAX);
        assert_eq!(hamming("00ff", "00ff00ff"), u32::MAX);
    }

    fn frame(at_ms: AtMs) -> FrameRecord {
        FrameRecord {
            file: format!("frames/f{at_ms}.jpg"),
            at_ms,
            epoch: at_ms,
            reason: FrameReason::Changed,
            phash: "0".repeat(16),
            width: 1280,
            height: 720,
            bytes: 1,
        }
    }

    #[test]
    fn nearest_picks_the_closest_frame_either_side() {
        let manifest = FrameManifest::new(vec![frame(0), frame(1_000), frame(5_000)]);
        assert_eq!(manifest.nearest(900).unwrap().at_ms, 1_000);
        assert_eq!(manifest.nearest(4_000).unwrap().at_ms, 5_000);
        assert!(FrameManifest::new(vec![]).nearest(0).is_none());
    }

    #[test]
    fn a_dense_window_is_thinned_but_keeps_its_endpoints() {
        let manifest = FrameManifest::new((0..100).map(|i| frame(i * 100)).collect());
        let picked = manifest.window(0, 10_000, 5);
        assert_eq!(picked.len(), 5);
        assert_eq!(picked[0].at_ms, 0);
        assert_eq!(picked[4].at_ms, 9_900);
    }

    #[test]
    fn truncation_is_recorded_so_missing_tail_frames_are_not_read_as_a_still_screen() {
        // Seen in a real 78-minute recording: the cap was reached at 60 minutes
        // and the last 18 had no frames at all.
        let manifest = FrameManifest::with_truncation(vec![frame(0), frame(60_000)], true);
        assert!(manifest.truncated);
        assert_eq!(manifest.covers_to_ms(), Some(60_000));

        let complete = FrameManifest::new(vec![frame(0)]);
        assert!(!complete.truncated);
        assert_eq!(FrameManifest::new(vec![]).covers_to_ms(), None);
    }

    #[test]
    fn a_sparse_window_returns_everything_in_range() {
        let manifest = FrameManifest::new(vec![frame(0), frame(1_000), frame(9_000)]);
        assert_eq!(manifest.window(0, 2_000, 10).len(), 2);
        assert!(manifest.window(20_000, 30_000, 10).is_empty());
    }
}
