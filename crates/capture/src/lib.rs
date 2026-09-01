//! Local signal capture for macOS.
//!
//! Everything in this crate runs on the user's machine and writes only into the
//! session folder. Nothing here opens a network connection — that is the whole
//! privacy story of the app, and keeping the one outbound door in a different
//! crate (`skillrec-agent`) is what makes that checkable rather than a claim.
//!
//! Each collector owns a thread and a poll interval chosen from what the signal
//! actually costs:
//!
//! | Collector | Interval | Why |
//! |---|---|---|
//! | [`window`] | 1000 ms, 1600 ms in a browser | Cheap — but the URL read behind it is not |
//! | [`clipboard`] | 700 ms | Cheap; a fast copy-then-paste must not be missed |
//! | [`screen`] | 1000 ms | The compositor handing over a framebuffer is the expensive one |

pub mod audio;
pub mod clipboard;
pub mod collector;
pub mod permissions;
pub mod screen;
pub mod url;
pub mod window;

pub use clipboard::ClipboardCollector;
pub use collector::{Collector, CollectorContext, CollectorHost};
pub use screen::ScreenCollector;
pub use window::ActiveWindowCollector;
