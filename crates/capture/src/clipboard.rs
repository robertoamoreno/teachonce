//! Clipboard tracking.
//!
//! What is captured is deliberately *not* the clipboard contents: it is the
//! available formats, the length, a short hash, and a one-line preview capped at
//! 120 characters. That is enough for the describer to tie a copy in one app to
//! a paste in another — which is the whole reason the signal exists — without
//! putting whatever you copied into a file and then into a prompt.
//!
//! The clipboard as it stands when the recording starts is read once as a
//! baseline and never emitted, so we capture what you copy *during* the session
//! rather than whatever happened to be there beforehand.

use std::time::Duration;

use skillrec_core::events::EventPayload;

use crate::collector::{Collector, CollectorContext};

/// macOS has no clipboard-change notification, so this is a poll. 700 ms is
/// fast enough that a copy immediately followed by a paste is still seen as two
/// separate moments.
const POLL: Duration = Duration::from_millis(700);

/// Preview length. Long enough to identify what was copied, short enough that a
/// copied document does not end up in the event log.
const PREVIEW_MAX: usize = 120;

/// Collapse whitespace and truncate on a character boundary.
pub fn preview(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= PREVIEW_MAX {
        return flat;
    }
    let truncated: String = flat.chars().take(PREVIEW_MAX).collect();
    format!("{truncated}…")
}

/// FNV-1a. This is a change signature and a copy/paste correlator, not a
/// security primitive — it never needs to resist an adversary, and using a
/// non-cryptographic hash keeps the crate free of a hashing dependency.
pub fn short_hash(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{hash:016x}")
}

/// What one clipboard read observed.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ClipboardSnapshot {
    pub formats: Vec<String>,
    pub length: usize,
    pub hash: String,
    pub text_preview: Option<String>,
}

impl ClipboardSnapshot {
    /// The comparison key. Includes the length and hash so that copying two
    /// different things of the same kind still registers as a change.
    pub fn signature(&self) -> String {
        format!("{}|{}|{}", self.formats.join(","), self.length, self.hash)
    }

    /// Nothing on the clipboard — a cleared clipboard is not a copy.
    pub fn is_empty(&self) -> bool {
        self.formats.is_empty()
    }

    pub fn from_text(text: &str) -> Self {
        Self {
            formats: vec!["text/plain".into()],
            length: text.len(),
            hash: short_hash(text.as_bytes()),
            text_preview: Some(preview(text)),
        }
    }

    pub fn from_image(width: usize, height: usize, bytes: &[u8]) -> Self {
        Self {
            formats: vec!["image/png".into()],
            length: bytes.len(),
            hash: short_hash(bytes),
            text_preview: Some(format!("[image {width}x{height}]")),
        }
    }
}

/// Emits `clipboard.change`.
pub struct ClipboardCollector {
    last_signature: String,
}

impl Default for ClipboardCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardCollector {
    pub fn new() -> Self {
        Self { last_signature: String::new() }
    }

    fn read(clipboard: &mut arboard::Clipboard) -> ClipboardSnapshot {
        if let Ok(text) = clipboard.get_text()
            && !text.is_empty()
        {
            return ClipboardSnapshot::from_text(&text);
        }
        if let Ok(image) = clipboard.get_image() {
            return ClipboardSnapshot::from_image(image.width, image.height, &image.bytes);
        }
        ClipboardSnapshot::default()
    }
}

impl Collector for ClipboardCollector {
    fn name(&self) -> &'static str {
        "clipboard"
    }

    fn run(&mut self, ctx: CollectorContext) {
        let mut clipboard = match arboard::Clipboard::new() {
            Ok(clipboard) => clipboard,
            Err(err) => {
                tracing::warn!(%err, "clipboard is unavailable; skipping the collector");
                return;
            }
        };

        // Baseline: whatever is already on the clipboard is pre-session data and
        // is never emitted.
        self.last_signature = Self::read(&mut clipboard).signature();

        loop {
            let snapshot = Self::read(&mut clipboard);
            let signature = snapshot.signature();
            if signature != self.last_signature {
                self.last_signature = signature;
                if !snapshot.is_empty() {
                    ctx.publish(
                        "clipboard",
                        EventPayload::ClipboardChange {
                            formats: snapshot.formats,
                            length: snapshot.length,
                            hash: snapshot.hash,
                            text_preview: snapshot.text_preview,
                        },
                    );
                }
            }
            if !ctx.sleep_or_stop(POLL) {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_collapse_whitespace_to_one_line() {
        assert_eq!(preview("  hello \n\t world  "), "hello world");
        assert_eq!(preview(""), "");
    }

    #[test]
    fn long_copies_are_truncated_with_an_ellipsis() {
        let long = "a".repeat(400);
        let out = preview(&long);
        assert_eq!(out.chars().count(), PREVIEW_MAX + 1);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncation_never_splits_a_multibyte_character() {
        // Byte-slicing here would panic; this is the regression that guards it.
        let emoji = "😀".repeat(200);
        let out = preview(&emoji);
        assert_eq!(out.chars().count(), PREVIEW_MAX + 1);
    }

    #[test]
    fn different_content_produces_a_different_signature() {
        let a = ClipboardSnapshot::from_text("acme corp");
        let b = ClipboardSnapshot::from_text("acme corp.");
        assert_ne!(a.signature(), b.signature());
        assert_eq!(a.signature(), ClipboardSnapshot::from_text("acme corp").signature());
    }

    #[test]
    fn an_empty_clipboard_is_not_a_copy() {
        assert!(ClipboardSnapshot::default().is_empty());
        assert!(!ClipboardSnapshot::from_text("x").is_empty());
    }

    #[test]
    fn images_are_described_by_size_not_content() {
        let snapshot = ClipboardSnapshot::from_image(800, 600, &[1, 2, 3]);
        assert_eq!(snapshot.text_preview.as_deref(), Some("[image 800x600]"));
        assert_eq!(snapshot.formats, vec!["image/png"]);
    }

    #[test]
    fn hashes_are_stable_and_distinguish_similar_input() {
        assert_eq!(short_hash(b"abc"), short_hash(b"abc"));
        assert_ne!(short_hash(b"abc"), short_hash(b"abd"));
        assert_eq!(short_hash(b"abc").len(), 16);
    }
}
