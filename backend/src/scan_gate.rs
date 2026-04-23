//! Coordination primitive that pauses background I/O work (library probes,
//! thumbnail generation) while any foreground playback/preparation work is active.
//!
//! ## Why this exists
//!
//! Both paths read from the same library mount, which is very often a network
//! share (SMB / NFS / sshfs; see `CLAUDE.md`). Concurrent ffprobe invocations
//! against a high-latency mount can saturate its read bandwidth — we've seen
//! individual probes take 18–38 seconds under contention — and when that
//! happens while the user is trying to watch something, the foreground ffmpeg
//! gets starved. The symptom was "playback still buffering": stats showed
//! `time=…` freezing for 10+ seconds at a stretch while four concurrent
//! probes hammered the mount, even for a pure `-c:v copy -c:a copy` remux
//! that should otherwise be bounded only by disk read speed.
//!
//! Giving the foreground session exclusive access to the mount fixes this.
//! Probes/thumbnails are batch work that can wait for the user to stop
//! watching; playback can't.
//!
//! ## Semantics
//!
//! `ScanGate::hold()` returns a guard that must be kept alive for the full
//! lifetime of the foreground session (drop semantics decrement the counter).
//! `ScanGate::wait_idle()` blocks until no guard is held. Both are cheap to
//! clone — the shared state lives behind an `Arc`.
//!
//! Currently-running probe jobs are NOT cancelled when a session starts; they
//! run to completion and the gate only affects the *next* job dequeued. That's
//! a deliberate trade-off — pre-empting an ffprobe mid-read wastes the work
//! already done, and on a healthy mount the probes are short. If probes turn
//! out to routinely take tens of seconds under contention, a future iteration
//! could add signal-based pre-emption (SIGSTOP/SIGCONT on the ffprobe child).

use std::sync::Arc;

use tokio::sync::watch;
use tracing::{debug, info};

/// Cheap-to-clone handle to the shared gate. Clone it into every background
/// worker that should pause for foreground playback/preparation, and into any
/// foreground owner that should hold the gate.
#[derive(Clone, Debug)]
pub struct ScanGate {
    tx: Arc<watch::Sender<u32>>,
}

impl Default for ScanGate {
    fn default() -> Self {
        Self::new()
    }
}

impl ScanGate {
    pub fn new() -> Self {
        // `watch` is the right channel here: each background worker wants to
        // block until the count drops to zero, and a broadcast/mpsc would
        // either miss updates or require manual re-subscription after each
        // transition. `watch::Receiver::changed()` cleanly handles the race
        // between "check the count" and "wait for next change".
        let (tx, _rx) = watch::channel(0u32);
        Self { tx: Arc::new(tx) }
    }

    /// Record the start of a foreground session. Hold the returned guard for
    /// the entire session lifetime — dropping it decrements the counter and,
    /// on the last drop, wakes every background worker currently waiting in
    /// `wait_idle`.
    pub fn hold(&self) -> ScanGateGuard {
        let new_count = self.tx.send_modify_and_read(|count| {
            *count += 1;
            *count
        });
        if new_count == 1 {
            info!("scan gate engaged; background probes/thumbnails paused");
        } else {
            debug!(active = new_count, "scan gate hold count incremented");
        }
        ScanGateGuard {
            tx: Arc::clone(&self.tx),
        }
    }

    /// Block until no foreground session is active. Returns immediately if
    /// the gate is already idle. Cheap to call per-job — workers should call
    /// this once before each unit of I/O-heavy work.
    pub async fn wait_idle(&self) {
        let mut rx = self.tx.subscribe();
        loop {
            let current = *rx.borrow_and_update();
            if current == 0 {
                return;
            }
            // Any transition (hold/release) wakes us; we loop and re-check.
            // An Err from `changed()` means the sender was dropped — the
            // gate is being torn down, so let the worker proceed rather than
            // hang forever.
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// RAII guard returned by `ScanGate::hold`. Dropping it decrements the gate's
/// counter and wakes waiters on the last drop. Intentionally non-`Clone`: the
/// count tracks individual foreground sessions, not handles to them.
#[derive(Debug)]
pub struct ScanGateGuard {
    tx: Arc<watch::Sender<u32>>,
}

impl Drop for ScanGateGuard {
    fn drop(&mut self) {
        let new_count = self.tx.send_modify_and_read(|count| {
            *count = count.saturating_sub(1);
            *count
        });
        if new_count == 0 {
            info!("scan gate released; background probes/thumbnails resuming");
        } else {
            debug!(active = new_count, "scan gate hold count decremented");
        }
    }
}

/// `watch::Sender::send_modify` returns `()`. We want the post-modification
/// value to log transitions without a second load. This shim does the
/// modification + read under the same internal lock.
trait SendModifyAndRead<T> {
    fn send_modify_and_read<F>(&self, modify: F) -> T
    where
        F: FnOnce(&mut T) -> T;
}

impl<T: Clone> SendModifyAndRead<T> for watch::Sender<T> {
    fn send_modify_and_read<F>(&self, modify: F) -> T
    where
        F: FnOnce(&mut T) -> T,
    {
        let mut captured: Option<T> = None;
        self.send_modify(|value| {
            captured = Some(modify(value));
        });
        // `modify` is always called by `send_modify`, so `captured` is
        // guaranteed Some. If that invariant ever changes, panicking here is
        // the right failure mode — we'd rather see it in tests than silently
        // emit a bogus count.
        captured.expect("send_modify closure must run")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn wait_idle_returns_immediately_when_no_holds() {
        let gate = ScanGate::new();
        // If this hangs, the test runner will catch it via cargo-test's
        // default timeout — but a short explicit bound makes failures
        // diagnosable without waiting minutes.
        timeout(Duration::from_millis(100), gate.wait_idle())
            .await
            .expect("wait_idle should not block when gate is idle");
    }

    #[tokio::test]
    async fn wait_idle_blocks_while_held_and_wakes_on_release() {
        let gate = ScanGate::new();
        let guard = gate.hold();

        // Spawn a waiter and confirm it's still pending after a short delay.
        let gate_clone = gate.clone();
        let waiter = tokio::spawn(async move { gate_clone.wait_idle().await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "waiter should still be blocked");

        drop(guard);

        // Release should wake the waiter promptly.
        timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter should resolve after guard drop")
            .expect("waiter task should not panic");
    }

    #[tokio::test]
    async fn multiple_holds_require_all_released() {
        let gate = ScanGate::new();
        let guard_a = gate.hold();
        let guard_b = gate.hold();

        let gate_clone = gate.clone();
        let waiter = tokio::spawn(async move { gate_clone.wait_idle().await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(guard_a);
        // Still one hold outstanding — waiter must still block.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !waiter.is_finished(),
            "waiter should block while any guard remains"
        );

        drop(guard_b);
        timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter should resolve after last guard drop")
            .expect("waiter task should not panic");
    }
}
