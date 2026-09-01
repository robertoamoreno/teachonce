//! The collector contract and the host that runs them.
//!
//! Every signal source is a blocking poll loop on its own OS thread. That is a
//! deliberate choice over async tasks: `xcap`, `arboard` and `osascript` are all
//! blocking, native, and occasionally slow, and one of them stalling must never
//! delay the others. Events flow out through an unbounded channel, so a collector
//! never blocks on the writer either — the recorder drains it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use skillrec_core::clock::EpochMs;
use skillrec_core::events::{EventInput, EventPayload};
use tokio::sync::mpsc::UnboundedSender;

/// What a collector is given when it starts.
#[derive(Clone)]
pub struct CollectorContext {
    tx: UnboundedSender<EventInput>,
    stop: Arc<AtomicBool>,
    session_dir: PathBuf,
    started_at: EpochMs,
}

impl CollectorContext {
    pub fn new(
        tx: UnboundedSender<EventInput>,
        stop: Arc<AtomicBool>,
        session_dir: PathBuf,
        started_at: EpochMs,
    ) -> Self {
        Self { tx, stop, session_dir, started_at }
    }

    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    pub fn started_at(&self) -> EpochMs {
        self.started_at
    }

    /// Emit an event. A closed channel means the recording already stopped, which
    /// is normal during teardown and not worth logging as an error.
    pub fn publish(&self, source: &'static str, payload: EventPayload) {
        let _ = self.tx.send(EventInput::new(source, payload));
    }

    pub fn should_stop(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Sleep until the interval elapses or the recording stops, whichever comes
    /// first. Returns `false` when it is time to exit.
    ///
    /// Polled in slices rather than one long sleep so that pressing Stop ends a
    /// 1.6-second browser poll immediately instead of a second and a half later.
    pub fn sleep_or_stop(&self, interval: Duration) -> bool {
        const SLICE: Duration = Duration::from_millis(50);
        let deadline = Instant::now() + interval;
        while Instant::now() < deadline {
            if self.should_stop() {
                return false;
            }
            std::thread::sleep(SLICE.min(deadline.saturating_duration_since(Instant::now())));
        }
        !self.should_stop()
    }
}

/// A signal source that runs for the length of one recording.
pub trait Collector: Send {
    /// Stable name, used as the event `source` and in logs.
    fn name(&self) -> &'static str;

    /// Poll until `ctx.should_stop()`. Implementations must not panic; a failing
    /// collector should log and either retry or return.
    fn run(&mut self, ctx: CollectorContext);

    /// Called once on the recorder thread after the loop ends, for collectors
    /// that must write a manifest or flush a file.
    fn finish(&mut self, _ctx: &CollectorContext) {}
}

/// Owns the collector threads for one recording.
pub struct CollectorHost {
    stop: Arc<AtomicBool>,
    handles: Vec<std::thread::JoinHandle<()>>,
}

impl CollectorHost {
    pub fn start(
        collectors: Vec<Box<dyn Collector>>,
        tx: UnboundedSender<EventInput>,
        session_dir: PathBuf,
        started_at: EpochMs,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        for mut collector in collectors {
            let ctx = CollectorContext::new(
                tx.clone(),
                Arc::clone(&stop),
                session_dir.clone(),
                started_at,
            );
            let name = collector.name();
            let handle = std::thread::Builder::new()
                .name(format!("collector-{name}"))
                .spawn(move || {
                    tracing::debug!(collector = name, "started");
                    collector.run(ctx.clone());
                    collector.finish(&ctx);
                    tracing::debug!(collector = name, "stopped");
                });
            match handle {
                Ok(handle) => handles.push(handle),
                Err(err) => tracing::warn!(collector = name, %err, "could not start collector"),
            }
        }
        Self { stop, handles }
    }

    /// Signal every collector to stop and wait for them to drain.
    ///
    /// Joining matters: a collector must finish writing its last frame or
    /// manifest before post-processing reads the session folder.
    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        for handle in self.handles {
            if handle.join().is_err() {
                tracing::warn!("a collector thread panicked during shutdown");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    struct Counter {
        ticks: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Collector for Counter {
        fn name(&self) -> &'static str {
            "counter"
        }

        fn run(&mut self, ctx: CollectorContext) {
            loop {
                self.ticks.fetch_add(1, Ordering::Relaxed);
                ctx.publish("counter", EventPayload::Marker { note: "tick".into() });
                if !ctx.sleep_or_stop(Duration::from_millis(20)) {
                    return;
                }
            }
        }
    }

    #[test]
    fn collectors_run_until_stopped_and_are_joined() {
        let (tx, mut rx) = unbounded_channel();
        let ticks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let host = CollectorHost::start(
            vec![Box::new(Counter { ticks: Arc::clone(&ticks) })],
            tx,
            PathBuf::from("/tmp"),
            0,
        );
        std::thread::sleep(Duration::from_millis(120));
        host.stop();

        let observed = ticks.load(Ordering::Relaxed);
        assert!(observed >= 2, "expected several ticks, saw {observed}");
        // stop() joined, so every event the collector produced is already queued.
        let mut drained = 0;
        while rx.try_recv().is_ok() {
            drained += 1;
        }
        assert_eq!(drained, observed);
    }

    #[test]
    fn a_pending_sleep_is_cut_short_by_stop() {
        let (tx, _rx) = unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = CollectorContext::new(tx, Arc::clone(&stop), PathBuf::from("/tmp"), 0);

        let flag = Arc::clone(&stop);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            flag.store(true, Ordering::Relaxed);
        });

        let started = Instant::now();
        // A 5s interval must not hold shutdown for 5 seconds.
        assert!(!ctx.sleep_or_stop(Duration::from_secs(5)));
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn publishing_after_the_receiver_is_gone_is_silent() {
        let (tx, rx) = unbounded_channel();
        let ctx = CollectorContext::new(tx, Arc::new(AtomicBool::new(false)), "/tmp".into(), 0);
        drop(rx);
        ctx.publish("test", EventPayload::Marker { note: "late".into() });
    }
}
