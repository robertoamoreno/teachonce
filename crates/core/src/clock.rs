//! One clock, used everywhere.
//!
//! Two time bases exist in a recording and confusing them is the classic source
//! of "the frame doesn't match the step" bugs:
//!
//! * **epoch ms** — wall-clock milliseconds since the Unix epoch. What collectors
//!   stamp events with, and the only base that can correlate two independent
//!   producers (the screen sampler and the clipboard poller).
//! * **`at_ms`** — milliseconds since the user pressed Record. The *only* base
//!   ever shown to the LLM, so the model never has to reason about absolute time.
//!
//! The conversion is a single subtraction against the session's `started_at`, and
//! it lives here so no other module reimplements it.

use std::time::{SystemTime, UNIX_EPOCH};

/// Wall-clock milliseconds since the Unix epoch.
pub type EpochMs = i64;

/// Milliseconds since the recording started (0 = the moment Record was pressed).
pub type AtMs = i64;

/// Current wall-clock time in milliseconds.
pub fn epoch_ms() -> EpochMs {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as EpochMs)
        .unwrap_or(0)
}

/// Convert a wall-clock stamp into session-relative time, clamped at zero.
#[inline]
pub fn to_at_ms(epoch: EpochMs, session_started_at: EpochMs) -> AtMs {
    (epoch - session_started_at).max(0)
}

/// Convert session-relative time back to wall clock.
#[inline]
pub fn to_epoch_ms(at: AtMs, session_started_at: EpochMs) -> EpochMs {
    session_started_at + at
}

/// Render a duration in milliseconds as a compact `m:ss` / `s.s` label for the UI
/// and for `description.md`.
pub fn format_span(ms: i64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let secs = ms as f64 / 1000.0;
    if secs < 60.0 {
        return format!("{secs:.1}s");
    }
    let mins = (secs / 60.0).floor() as i64;
    let rem = (secs - (mins as f64 * 60.0)).round() as i64;
    format!("{mins}m{rem:02}s")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_ms_is_relative_and_never_negative() {
        assert_eq!(to_at_ms(1_500, 1_000), 500);
        // A collector whose stamp predates the session start clamps to 0 rather
        // than emitting a negative offset the model would have to interpret.
        assert_eq!(to_at_ms(900, 1_000), 0);
    }

    #[test]
    fn epoch_round_trips_through_at_ms() {
        let started = 1_700_000_000_000;
        assert_eq!(to_epoch_ms(to_at_ms(started + 42, started), started), started + 42);
    }

    #[test]
    fn spans_render_at_human_scale() {
        assert_eq!(format_span(400), "400ms");
        assert_eq!(format_span(2_500), "2.5s");
        assert_eq!(format_span(95_000), "1m35s");
    }
}
