//! Keeps a session's video going through GPU device loss, driver resets,
//! resolution changes and the source moving to another GPU, without losing
//! the replay buffer.
//!
//! Only the video pipeline restarts, under a new `segment` number; audio and
//! the buffer carry on. The saver writes one segment per clip, so a clip
//! that spans a restart starts after it. That costs the seconds before the
//! restart, but the alternative is worse: before this, a lost device ended
//! the session and threw the whole buffer away.
//!
//! - **Failures** (device removed or hung, a driver update, any error that
//!   stops the encode thread): retried after [`RETRY_DELAYS`], about 40 s
//!   in all, which covers a driver reinstall. A pipeline that then runs for
//!   [`HEALTHY_AFTER`] earns a fresh set of retries.
//! - **Resolution changes**: the pipeline keeps running at its first size
//!   and scales the picture to fit, so a game that starts in a small window
//!   and then goes fullscreen at 1440p would be recorded at the window's
//!   size. Once the new size has held for [`SETTLE`] (so dragging a window
//!   edge doesn't restart on every step), the pipeline restarts at it.
//! - **Another GPU**: the pipeline runs on the GPU driving the source's
//!   monitor (cheapest copy). When the window moves to a monitor on another
//!   GPU, or a hybrid laptop switches its panel to the other GPU (Advanced
//!   Optimus, AMD Smart Access Graphics), the pipeline follows once the
//!   change has held for [`SETTLE`].
//! - **HDR switched** on or off for the source's display (Win+Alt+B, or the
//!   window moved to a display in the other mode): the capture format and
//!   the encoded colour depend on it, so the pipeline restarts in the new
//!   mode once it has held for [`SETTLE`].

use std::fmt;

use rec_core::time::{HNS_PER_SEC, Hns};

/// Waits before each restart attempt after a failure.
pub const RETRY_DELAYS: [Hns; 5] = [
    HNS_PER_SEC,
    2 * HNS_PER_SEC,
    5 * HNS_PER_SEC,
    10 * HNS_PER_SEC,
    20 * HNS_PER_SEC,
];
/// A pipeline that has run this long gets the full set of retries again.
pub const HEALTHY_AFTER: Hns = 60 * HNS_PER_SEC;
/// A new size or GPU must hold this long before the pipeline restarts.
pub const SETTLE: Hns = 2 * HNS_PER_SEC;
/// Sizes below this are a minimized or hidden window, not a resolution.
pub const MIN_SIDE: u32 = 64;

/// Why a running pipeline should restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// The encoded size the source now calls for.
    Resized { from: (u32, u32), to: (u32, u32) },
    /// The source is now shown by another GPU.
    Adapter,
    /// HDR is now on (or off) for the source's display.
    Hdr { on: bool },
}

/// What changed about the display showing the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayChange {
    /// It is driven by another GPU now.
    Adapter,
    /// Its HDR mode differs from the pipeline's.
    Hdr { on: bool },
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Change::Resized { from, to } => {
                write!(f, "size {}x{} -> {}x{}", from.0, from.1, to.0, to.1)
            }
            Change::Adapter => f.write_str("the source moved to another GPU"),
            Change::Hdr { on: true } => f.write_str("HDR was turned on for the display"),
            Change::Hdr { on: false } => f.write_str("HDR was turned off for the display"),
        }
    }
}

/// Restart decisions for one session's video pipeline.
#[derive(Clone, Debug)]
pub struct Recovery {
    /// Failures since the last healthy run.
    failures: usize,
    /// When to try starting again; `Some` while no pipeline runs.
    retry_at: Option<Hns>,
    /// When the running pipeline started.
    started: Hns,
    /// A change seen and since when; it must hold for [`SETTLE`].
    pending: Option<(Pending, Hns)>,
    /// The size the last resize restart aimed for.
    resized_to: Option<(u32, u32)>,
    /// A size a restart did not reach (the source reports one size and the
    /// pipeline opens at another): not chased again until it changes.
    unreachable: Option<(u32, u32)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    Size((u32, u32)),
    Display(DisplayChange),
}

impl Recovery {
    /// For a pipeline that started at `now`.
    pub fn new(now: Hns) -> Self {
        Self {
            failures: 0,
            retry_at: None,
            started: now,
            pending: None,
            resized_to: None,
            unreachable: None,
        }
    }

    /// A pipeline started at `now`, encoding at `encoded`.
    pub fn started(&mut self, now: Hns, encoded: (u32, u32)) {
        self.retry_at = None;
        self.started = now;
        self.pending = None;
        if let Some(target) = self.resized_to.take() {
            self.unreachable = (target != encoded).then_some(target);
        }
    }

    /// The running pipeline stopped with an error. Returns when to try
    /// again, or `None` to give up.
    pub fn crashed(&mut self, now: Hns) -> Option<Hns> {
        if now - self.started >= HEALTHY_AFTER {
            self.failures = 0;
        }
        self.failed(now)
    }

    /// A pipeline could not be started. Returns when to try again, or
    /// `None` to give up.
    pub fn start_failed(&mut self, now: Hns) -> Option<Hns> {
        self.failed(now)
    }

    fn failed(&mut self, now: Hns) -> Option<Hns> {
        self.pending = None;
        self.resized_to = None;
        self.retry_at = RETRY_DELAYS.get(self.failures).map(|d| now + d);
        self.failures += 1;
        self.retry_at
    }

    pub fn retry_due(&self, now: Hns) -> bool {
        self.retry_at.is_some_and(|at| now >= at)
    }

    /// Whether the running pipeline, encoding a picture of `encoded` size,
    /// should restart because the source now calls for `wanted`, or its
    /// display changed. A display change wins over a size change: the
    /// restart picks up the new size anyway.
    pub fn check(
        &mut self,
        now: Hns,
        encoded: (u32, u32),
        wanted: Option<(u32, u32)>,
        display: Option<DisplayChange>,
    ) -> Option<Change> {
        if wanted != self.unreachable {
            self.unreachable = None;
        }
        let size = wanted.filter(|&(w, h)| {
            (w, h) != encoded && w >= MIN_SIDE && h >= MIN_SIDE && Some((w, h)) != self.unreachable
        });
        let seen = if let Some(change) = display {
            Pending::Display(change)
        } else if let Some(size) = size {
            Pending::Size(size)
        } else {
            self.pending = None;
            return None;
        };
        match self.pending {
            Some((pending, since)) if pending == seen => {
                if now - since < SETTLE {
                    return None;
                }
                self.pending = None;
                Some(match seen {
                    Pending::Display(DisplayChange::Adapter) => Change::Adapter,
                    Pending::Display(DisplayChange::Hdr { on }) => Change::Hdr { on },
                    Pending::Size(to) => {
                        self.resized_to = Some(to);
                        Change::Resized { from: encoded, to }
                    }
                })
            }
            _ => {
                self.pending = Some((seen, now));
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Hns = HNS_PER_SEC;
    const HD: (u32, u32) = (1920, 1080);
    const QHD: (u32, u32) = (2560, 1440);

    #[test]
    fn failures_retry_with_backoff_then_give_up() {
        let mut r = Recovery::new(0);
        let mut now = 10 * S;
        assert_eq!(r.crashed(now), Some(now + S));
        assert!(!r.retry_due(now));
        for delay in &RETRY_DELAYS[1..] {
            now += 30 * S;
            assert!(r.retry_due(now));
            assert_eq!(r.start_failed(now), Some(now + delay));
        }
        assert_eq!(r.start_failed(now + 30 * S), None);
        assert!(!r.retry_due(now + 1000 * S));
    }

    #[test]
    fn a_healthy_run_earns_fresh_retries() {
        let mut r = Recovery::new(0);
        for _ in 0..4 {
            r.start_failed(0);
        }
        r.started(S, HD);
        // Crashing soon after starting continues the count...
        assert_eq!(r.crashed(10 * S), Some(10 * S + RETRY_DELAYS[4]));
        r.started(40 * S, HD);
        // ...but after a minute of running it starts over.
        assert_eq!(r.crashed(200 * S), Some(200 * S + RETRY_DELAYS[0]));
    }

    #[test]
    fn a_new_size_must_settle_before_a_restart() {
        let mut r = Recovery::new(0);
        assert_eq!(r.check(2 * S, HD, Some(HD), None), None);
        // Dragging a window edge: the size keeps changing.
        assert_eq!(r.check(4 * S, HD, Some((1000, 700)), None), None);
        assert_eq!(r.check(6 * S, HD, Some((1100, 750)), None), None);
        // It holds.
        assert_eq!(r.check(8 * S, HD, Some(QHD), None), None);
        assert_eq!(
            r.check(10 * S, HD, Some(QHD), None),
            Some(Change::Resized { from: HD, to: QHD })
        );
        r.started(10 * S, QHD);
        assert_eq!(r.check(12 * S, QHD, Some(QHD), None), None);
    }

    #[test]
    fn going_back_cancels_a_pending_resize() {
        let mut r = Recovery::new(0);
        assert_eq!(r.check(2 * S, HD, Some(QHD), None), None);
        assert_eq!(r.check(4 * S, HD, Some(HD), None), None);
        assert_eq!(r.check(6 * S, HD, Some(QHD), None), None);
    }

    #[test]
    fn minimized_windows_and_no_frames_are_ignored() {
        let mut r = Recovery::new(0);
        for t in 1..10 {
            assert_eq!(r.check(t * S, HD, Some((160, 28)), None), None);
            assert_eq!(r.check(t * S, HD, None, None), None);
        }
    }

    #[test]
    fn a_size_the_restart_did_not_reach_is_not_chased() {
        let mut r = Recovery::new(0);
        r.check(0, HD, Some(QHD), None);
        assert!(r.check(2 * S, HD, Some(QHD), None).is_some());
        // The new pipeline opened at another size than the frames report.
        r.started(2 * S, (2560, 1400));
        for t in 3..20 {
            assert_eq!(r.check(t * S, (2560, 1400), Some(QHD), None), None);
        }
        // A different size later is followed again.
        r.check(20 * S, (2560, 1400), Some(HD), None);
        assert!(r.check(22 * S, (2560, 1400), Some(HD), None).is_some());
    }

    #[test]
    fn switching_hdr_restarts_once_settled() {
        let mut r = Recovery::new(0);
        let on = Some(DisplayChange::Hdr { on: true });
        assert_eq!(r.check(S, HD, Some(HD), on), None);
        assert_eq!(r.check(2 * S, HD, Some(HD), on), None);
        assert_eq!(
            r.check(3 * S, HD, Some(HD), on),
            Some(Change::Hdr { on: true })
        );
        // Toggled straight back: nothing to do.
        let mut r = Recovery::new(0);
        assert_eq!(r.check(S, HD, Some(HD), on), None);
        assert_eq!(r.check(2 * S, HD, Some(HD), None), None);
        assert_eq!(r.check(4 * S, HD, Some(HD), None), None);
        // A display change wins over a pending size change.
        let mut r = Recovery::new(0);
        assert_eq!(r.check(S, HD, Some(QHD), None), None);
        assert_eq!(r.check(2 * S, HD, Some(QHD), on), None);
        assert_eq!(
            r.check(4 * S, HD, Some(QHD), on),
            Some(Change::Hdr { on: true })
        );
    }

    #[test]
    fn moving_to_another_gpu_restarts_once_settled() {
        let mut r = Recovery::new(0);
        assert_eq!(
            r.check(2 * S, HD, Some(HD), Some(DisplayChange::Adapter)),
            None
        );
        assert_eq!(
            r.check(4 * S, HD, Some(HD), Some(DisplayChange::Adapter)),
            Some(Change::Adapter)
        );
        // A brief flip back and forth does not.
        let mut r = Recovery::new(0);
        assert_eq!(
            r.check(2 * S, HD, Some(HD), Some(DisplayChange::Adapter)),
            None
        );
        assert_eq!(r.check(4 * S, HD, Some(HD), None), None);
        assert_eq!(
            r.check(6 * S, HD, Some(HD), Some(DisplayChange::Adapter)),
            None
        );
    }
}
