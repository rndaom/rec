//! Adaptive GPU priority (decision D15): rec's GPU work (the capture copy and
//! the colour conversion) runs at normal priority, like every other app,
//! and is raised only while a game that saturates the GPU starves it badly
//! enough to drop frames, and only for as long as raising it helps.
//!
//! Once a second the encode thread hands the [`Controller`] a [`Second`]:
//! how many encode ticks there were and how many of them were *starved*:
//! a frame was dropped because no converted surface was free or the
//! encoder could not take input, or rec's GPU work queued [`LATE_PERIODS`]
//! frame periods ago is still unfinished (the backlog that leads to those
//! drops). Work merely one period late is normal under a GPU-bound game:
//! the GPU finishes the game's current command buffer before it runs ours
//! (measured: up to one game frame, whatever the priority), and the
//! encoder's queue absorbs it. Late wake-ups of the encode thread are a CPU
//! problem and repeated frames (a game rendering below the encode rate) are
//! not a problem, so neither counts.
//!
//! The rules, chosen to react within seconds but never flap:
//! - **Raise** when 3 of the last 5 seconds were bad (at least 5% of the
//!   ticks and at least 2 starved). A single hitch never raises.
//! - **Give up** when, of the first [`TRIAL`] seconds at high priority,
//!   half or more were still bad: priority does not help this load (the
//!   measured case with hardware GPU scheduling on), so it goes back to
//!   normal and stays there for [`MAX_HOLD`].
//! - **Lower** after holding for at least the hold time (from [`MIN_HOLD`])
//!   and the last [`CLEAN_SECONDS`] were clean. At high priority the work
//!   is healthy whatever the game does, so lowering is really a probe:
//!   "does it still starve at normal?".
//! - **Back off**: a raise within [`RELAPSE`] of the last lowering doubles
//!   the hold (up to [`MAX_HOLD`]); staying lowered for [`SETTLED`] resets
//!   it. Under a load that persists, a failed probe costs ~3 s of drops,
//!   at most every 10 minutes.

use rec_core::time::{HNS_PER_SEC, Hns};

/// `IDXGIDevice::SetGPUThreadPriority` value of every D3D device by default.
pub(crate) const NORMAL: i32 = 0;
/// The raised value: the top of the relative range (-7..=7). It needs no
/// privilege and stays in the normal scheduling band; the absolute
/// soft-realtime levels made latency worse in measurements and hard
/// realtime is refused without privilege (D15).
pub(crate) const HIGH: i32 = 7;

/// GPU work unfinished this many frame periods after it was queued is a
/// starved tick: half the surface pool, well past one frame of a playable
/// game (50 ms at 60 fps).
pub(crate) const LATE_PERIODS: i64 = 3;
/// Seconds of history the raise rule looks at.
const WINDOW: u32 = 5;
/// Bad seconds within [`WINDOW`] that raise the priority.
const RAISE_AFTER: u32 = 3;
/// Seconds at high priority after which it is judged.
pub(crate) const TRIAL: u32 = 10;
/// Clean seconds in a row needed before a probe at normal priority.
pub(crate) const CLEAN_SECONDS: u32 = 10;
/// Shortest time at high priority.
pub(crate) const MIN_HOLD: Hns = 30 * HNS_PER_SEC;
/// Longest time at high priority between probes, and how long a failed
/// trial keeps the priority normal.
pub(crate) const MAX_HOLD: Hns = 600 * HNS_PER_SEC;
/// A raise this soon after lowering means the probe failed.
pub(crate) const RELAPSE: Hns = 60 * HNS_PER_SEC;
/// Staying at normal this long forgets earlier failed probes.
pub(crate) const SETTLED: Hns = 300 * HNS_PER_SEC;

/// One second of encode ticks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Second {
    /// Ticks since the first captured frame.
    pub ticks: u32,
    /// Of those, ticks with a frame dropped for want of a converted
    /// picture, or with GPU work [`LATE_PERIODS`] behind.
    pub starved: u32,
}

impl Second {
    /// Clearly visible stutter: at least 5% of the ticks, and more than one.
    fn bad(self) -> bool {
        self.starved >= 2 && self.starved * 20 >= self.ticks
    }

    /// At most 1% of the ticks (none at 60 fps).
    fn clean(self) -> bool {
        self.starved * 100 <= self.ticks
    }
}

/// What the controller decided, with a reason for the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Raise(String),
    /// Back to normal: healthy at high priority (a probe), or high priority
    /// did not help (a failed trial).
    Lower(String),
}

/// The adaptive policy; pure logic, driven by [`Controller::observe`].
#[derive(Clone, Debug)]
pub(crate) struct Controller {
    raised: bool,
    /// The last [`WINDOW`] seconds while at normal priority, newest last.
    recent: Vec<Second>,
    /// When the priority last changed.
    since: Hns,
    /// Seconds at high priority so far, and how many of them were bad.
    raised_secs: u32,
    raised_bad: u32,
    /// Clean seconds in a row while raised.
    clean_run: u32,
    hold: Hns,
    lowered_at: Option<Hns>,
    /// No raising before this (after a failed trial).
    paused_until: Option<Hns>,
}

impl Controller {
    pub(crate) fn new(now: Hns) -> Self {
        Self {
            raised: false,
            recent: Vec::with_capacity(WINDOW as usize),
            since: now,
            raised_secs: 0,
            raised_bad: 0,
            clean_run: 0,
            hold: MIN_HOLD,
            lowered_at: None,
            paused_until: None,
        }
    }

    #[cfg(test)]
    fn raised(&self) -> bool {
        self.raised
    }

    /// Feeds one second ending at `now`; returns a change to apply.
    pub(crate) fn observe(&mut self, now: Hns, s: Second) -> Option<Decision> {
        if self.raised {
            return self.observe_raised(now, s);
        }
        if self.paused_until.is_some_and(|until| now < until) {
            return None;
        }
        self.paused_until = None;
        if self.recent.len() == WINDOW as usize {
            self.recent.remove(0);
        }
        self.recent.push(s);
        if self
            .lowered_at
            .is_some_and(|at| now - at >= SETTLED && self.hold != MIN_HOLD)
        {
            self.hold = MIN_HOLD;
        }
        let bad = self.recent.iter().filter(|s| s.bad()).count() as u32;
        if bad < RAISE_AFTER {
            return None;
        }
        let relapse = self.lowered_at.is_some_and(|at| now - at < RELAPSE);
        if relapse {
            self.hold = (self.hold * 2).min(MAX_HOLD);
        }
        let (ticks, starved) = self
            .recent
            .iter()
            .fold((0, 0), |(t, s), x| (t + x.ticks, s + x.starved));
        self.recent.clear();
        self.raised = true;
        self.since = now;
        self.raised_secs = 0;
        self.raised_bad = 0;
        self.clean_run = 0;
        Some(Decision::Raise(format!(
            "{starved} of {ticks} frames starved in the last {WINDOW} s{}",
            if relapse { " right after lowering" } else { "" },
        )))
    }

    fn observe_raised(&mut self, now: Hns, s: Second) -> Option<Decision> {
        self.raised_secs += 1;
        self.raised_bad += u32::from(s.bad());
        self.clean_run = if s.clean() { self.clean_run + 1 } else { 0 };
        if self.raised_secs == TRIAL && self.raised_bad * 2 >= TRIAL {
            self.lower(now);
            self.paused_until = Some(now + MAX_HOLD);
            // The failed trial says nothing about the probe interval.
            self.lowered_at = None;
            return Some(Decision::Lower(format!(
                "high priority did not help ({} of {TRIAL} s still starved); normal for the next {} s",
                self.raised_bad,
                MAX_HOLD / HNS_PER_SEC
            )));
        }
        if now - self.since < self.hold || self.clean_run < CLEAN_SECONDS {
            return None;
        }
        let held = (now - self.since) / HNS_PER_SEC;
        let clean = self.clean_run;
        self.lower(now);
        Some(Decision::Lower(format!(
            "no starved frames for {clean} s after {held} s at high priority"
        )))
    }

    fn lower(&mut self, now: Hns) {
        self.raised = false;
        self.since = now;
        self.lowered_at = Some(now);
        self.recent.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Hns = HNS_PER_SEC;

    /// Drives `c` with `secs` seconds of `s`, from `*t`; returns the
    /// decisions with the second they came at.
    fn run(c: &mut Controller, t: &mut Hns, secs: u32, s: Second) -> Vec<(Hns, Decision)> {
        let mut out = Vec::new();
        for _ in 0..secs {
            *t += S;
            if let Some(d) = c.observe(*t, s) {
                out.push((*t / S, d));
            }
        }
        out
    }

    const HEALTHY: Second = Second {
        ticks: 60,
        starved: 0,
    };
    const STARVED: Second = Second {
        ticks: 60,
        starved: 12,
    };
    /// Under 5%: occasional hitches.
    const HITCHY: Second = Second {
        ticks: 60,
        starved: 2,
    };

    fn raises(d: &[(Hns, Decision)]) -> Vec<Hns> {
        d.iter()
            .filter(|(_, d)| matches!(d, Decision::Raise(_)))
            .map(|(t, _)| *t)
            .collect()
    }

    fn lowers(d: &[(Hns, Decision)]) -> Vec<Hns> {
        d.iter()
            .filter(|(_, d)| matches!(d, Decision::Lower(_)))
            .map(|(t, _)| *t)
            .collect()
    }

    #[test]
    fn classifies_seconds() {
        assert!(STARVED.bad() && !STARVED.clean());
        assert!(!HITCHY.bad() && !HITCHY.clean());
        assert!(!HEALTHY.bad() && HEALTHY.clean());
        // One late frame is never bad, even at a low rate.
        assert!(
            !Second {
                ticks: 10,
                starved: 1
            }
            .bad()
        );
        assert!(
            Second {
                ticks: 30,
                starved: 2
            }
            .bad()
        );
        // At 144 fps one late frame is still clean.
        assert!(
            Second {
                ticks: 144,
                starved: 1
            }
            .clean()
        );
        // No ticks (nothing captured yet) is clean, never bad.
        assert!(Second::default().clean() && !Second::default().bad());
    }

    #[test]
    fn healthy_and_hitchy_capture_never_raises() {
        let mut c = Controller::new(0);
        let mut t = 0;
        assert!(run(&mut c, &mut t, 600, HEALTHY).is_empty());
        assert!(run(&mut c, &mut t, 600, HITCHY).is_empty());
        // Isolated bad seconds, two in every five, are tolerated.
        for _ in 0..100 {
            assert!(run(&mut c, &mut t, 1, STARVED).is_empty());
            assert!(run(&mut c, &mut t, 2, HEALTHY).is_empty());
            assert!(run(&mut c, &mut t, 1, STARVED).is_empty());
            assert!(run(&mut c, &mut t, 1, HEALTHY).is_empty());
        }
        assert!(!c.raised());
    }

    #[test]
    fn sustained_starvation_raises_within_three_seconds() {
        let mut c = Controller::new(0);
        let mut t = 0;
        run(&mut c, &mut t, 20, HEALTHY);
        let d = run(&mut c, &mut t, 3, STARVED);
        assert_eq!(raises(&d), [23]);
        assert!(c.raised());
        let Decision::Raise(why) = &d[0].1 else {
            unreachable!()
        };
        assert!(why.starts_with("36 of 300 frames starved"), "{why}");
        // Also three bad seconds with good ones in between.
        let mut c = Controller::new(0);
        let mut t = 0;
        run(&mut c, &mut t, 1, STARVED);
        run(&mut c, &mut t, 1, HEALTHY);
        run(&mut c, &mut t, 1, STARVED);
        run(&mut c, &mut t, 1, HEALTHY);
        assert_eq!(raises(&run(&mut c, &mut t, 1, STARVED)), [5]);
    }

    #[test]
    fn holds_then_lowers_once_clean() {
        let mut c = Controller::new(0);
        let mut t = 0;
        run(&mut c, &mut t, 3, STARVED);
        assert!(c.raised());
        // Healthy at high priority: lowered after the minimum hold.
        let d = run(&mut c, &mut t, 60, HEALTHY);
        assert_eq!(lowers(&d), [3 + MIN_HOLD / S]);
        assert!(!c.raised());
    }

    #[test]
    fn stays_raised_while_not_clean() {
        let mut c = Controller::new(0);
        let mut t = 0;
        run(&mut c, &mut t, 3, STARVED);
        // Hitches at high priority keep it raised past the hold...
        assert!(run(&mut c, &mut t, 120, HITCHY).is_empty());
        // ...and it lowers only after a full clean run.
        let d = run(&mut c, &mut t, CLEAN_SECONDS, HEALTHY);
        assert_eq!(lowers(&d), [3 + 120 + Hns::from(CLEAN_SECONDS)]);
    }

    #[test]
    fn gives_up_when_high_priority_does_not_help() {
        let mut c = Controller::new(0);
        let mut t = 0;
        run(&mut c, &mut t, 3, STARVED);
        // Still starved at high priority: back to normal after the trial,
        // and no new raise for MAX_HOLD however bad it gets.
        let d = run(&mut c, &mut t, (MAX_HOLD / S) as u32, STARVED);
        assert_eq!(lowers(&d), [3 + Hns::from(TRIAL)]);
        assert!(raises(&d).is_empty());
        let Decision::Lower(why) = &d[0].1 else {
            unreachable!()
        };
        assert!(why.starts_with("high priority did not help"), "{why}");
        // Then it tries again.
        let d = run(&mut c, &mut t, 20, STARVED);
        assert_eq!(raises(&d).len(), 1);
        // Half bad is a failed trial too; fewer is not.
        let mut c = Controller::new(0);
        let mut t = 0;
        run(&mut c, &mut t, 3, STARVED);
        for _ in 0..TRIAL / 2 {
            run(&mut c, &mut t, 1, STARVED);
            run(&mut c, &mut t, 1, HEALTHY);
        }
        assert!(!c.raised());
        let mut c = Controller::new(0);
        let mut t = 0;
        run(&mut c, &mut t, 3, STARVED);
        run(&mut c, &mut t, TRIAL / 2 - 1, STARVED);
        run(&mut c, &mut t, TRIAL / 2 + 1, HEALTHY);
        assert!(c.raised());
    }

    #[test]
    fn failed_probes_back_off_exponentially() {
        let mut c = Controller::new(0);
        let mut t = 0;
        // A persistent load: starved whenever at normal priority, healthy at
        // high. Simulate by feeding what the current priority would see.
        let mut changes = Vec::new();
        for _ in 0..3600 {
            let s = if c.raised() { HEALTHY } else { STARVED };
            t += S;
            if let Some(d) = c.observe(t, s) {
                changes.push((t / S, d));
            }
        }
        let up = raises(&changes);
        let down = lowers(&changes);
        // Hold doubles after every failed probe: 30, 60, 120, 240, 480, 600.
        let holds: Vec<Hns> = up.iter().zip(&down).map(|(u, d)| d - u).collect();
        assert_eq!(holds, [30, 60, 120, 240, 480, 600, 600, 600, 600]);
        // Each probe costs 3 s at normal before the raise.
        for (d, u) in down.iter().zip(&up[1..]) {
            assert_eq!(u - d, 3);
        }
        // Time spent starved over the hour: 3 s at start + 3 s per probe.
        let starved_secs = 3 + 3 * down.len() as Hns;
        assert!(starved_secs <= 30, "{starved_secs}");
    }

    #[test]
    fn a_settled_normal_period_resets_the_backoff() {
        let mut c = Controller::new(0);
        let mut t = 0;
        // Two failed probes: the hold is now 120 s.
        run(&mut c, &mut t, 3, STARVED);
        run(&mut c, &mut t, 30, HEALTHY);
        run(&mut c, &mut t, 3, STARVED);
        run(&mut c, &mut t, 60, HEALTHY);
        run(&mut c, &mut t, 3, STARVED);
        assert_eq!(c.hold, 4 * MIN_HOLD);
        run(&mut c, &mut t, 120, HEALTHY);
        assert!(!c.raised());
        // The game stops saturating the GPU for a while.
        run(&mut c, &mut t, (SETTLED / S) as u32, HEALTHY);
        assert_eq!(c.hold, MIN_HOLD);
        // A new load is held only the minimum again.
        let raised_at = t / S + 3;
        run(&mut c, &mut t, 3, STARVED);
        let d = run(&mut c, &mut t, 60, HEALTHY);
        assert_eq!(lowers(&d), [raised_at + MIN_HOLD / S]);
    }

    #[test]
    fn a_late_relapse_does_not_double_the_hold() {
        let mut c = Controller::new(0);
        let mut t = 0;
        run(&mut c, &mut t, 3, STARVED);
        run(&mut c, &mut t, 30, HEALTHY);
        assert!(!c.raised());
        // Starving again two minutes later is a new load, not a failed probe.
        run(&mut c, &mut t, 120, HEALTHY);
        run(&mut c, &mut t, 3, STARVED);
        assert!(c.raised());
        assert_eq!(c.hold, MIN_HOLD);
    }
}

#[cfg(windows)]
pub(crate) use imp::Control;

#[cfg(windows)]
mod imp {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use rec_core::time::Hns;
    use windows::Win32::Foundation::{FALSE, TRUE};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_ASYNC_GETDATA_DONOTFLUSH, D3D11_QUERY_DESC, D3D11_QUERY_EVENT, ID3D11DeviceContext,
        ID3D11Query,
    };
    use windows::Win32::Graphics::Dxgi::IDXGIDevice;
    use windows::core::{BOOL, Interface};

    use super::{Controller, Decision, HIGH, LATE_PERIODS, NORMAL, Second};
    use crate::d3d::{self, Gpu};
    use crate::util::Context;
    use crate::{GpuPriority, GpuPriorityChange, SharedStats, VideoError};

    /// Event queries in flight: one per converted tick, so this many ticks
    /// of GPU backlog are measured, comfortably past [`LATE_PERIODS`].
    const QUERIES: usize = 8;

    /// A GPU fence on the conversion of one tick.
    #[derive(Debug)]
    struct Fence {
        query: ID3D11Query,
        /// When it was queued; `None` = free.
        issued: Option<Hns>,
        /// Already counted as late.
        late: bool,
    }

    /// The encode thread's GPU priority: measures how late its GPU work
    /// runs and applies the mode (and, when adaptive, the [`Controller`]'s
    /// decisions) to the device.
    #[derive(Debug)]
    pub(crate) struct Control {
        mode: GpuPriority,
        dxgi: IDXGIDevice,
        controller: Controller,
        level: i32,
        fences: Vec<Fence>,
        second: Second,
        stats: Arc<SharedStats>,
        /// " (hardware GPU scheduling on)" or similar, for the log: what
        /// priority can do differs with it (D15).
        scheduling: &'static str,
    }

    impl Control {
        pub(crate) fn new(
            gpu: &Gpu,
            mode: GpuPriority,
            now: Hns,
            stats: Arc<SharedStats>,
        ) -> Result<Self, VideoError> {
            let dxgi: IDXGIDevice = gpu.device.cast().ctx("IDXGIDevice")?;
            let desc = D3D11_QUERY_DESC {
                Query: D3D11_QUERY_EVENT,
                MiscFlags: 0,
            };
            let mut fences = Vec::with_capacity(QUERIES);
            for _ in 0..QUERIES {
                let mut query = None;
                // SAFETY: valid descriptor; out-pointer valid for writes.
                unsafe { gpu.device.CreateQuery(&desc, Some(&mut query)) }
                    .ctx("CreateQuery(event)")?;
                let query = query.ok_or_else(|| VideoError::Other("CreateQuery: null".into()))?;
                fences.push(Fence {
                    query,
                    issued: None,
                    late: false,
                });
            }
            let mut control = Self {
                mode,
                dxgi,
                controller: Controller::new(now),
                level: NORMAL,
                fences,
                second: Second::default(),
                stats,
                scheduling: match d3d::hardware_scheduling(gpu.luid) {
                    Some(true) => " (hardware GPU scheduling on)",
                    Some(false) => " (hardware GPU scheduling off)",
                    None => "",
                },
            };
            if mode == GpuPriority::High {
                let why = format!("gpu_priority = high{}", control.scheduling);
                control.apply(now, HIGH, why);
            }
            Ok(control)
        }

        /// Checks the fences at the start of a tick (after the first
        /// frame): returns whether some conversion is still unfinished
        /// [`LATE_PERIODS`] `period`s after it was queued. Each late
        /// conversion counts once in [`SharedStats::late`].
        pub(crate) fn late_work(
            &mut self,
            context: &ID3D11DeviceContext,
            now: Hns,
            period: Hns,
        ) -> bool {
            let mut late = false;
            for fence in &mut self.fences {
                let Some(issued) = fence.issued else {
                    continue;
                };
                let mut done = FALSE;
                // SAFETY: an event query writes one BOOL, and only when it
                // is done (S_OK); DONOTFLUSH because `fence` flushed.
                let _ = unsafe {
                    context.GetData(
                        &fence.query,
                        Some((&raw mut done).cast()),
                        size_of::<BOOL>() as u32,
                        D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
                    )
                };
                if done == TRUE {
                    fence.issued = None;
                    fence.late = false;
                } else if now - issued >= LATE_PERIODS * period {
                    late = true;
                    if !fence.late {
                        fence.late = true;
                        self.stats.late.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            late
        }

        /// Queues a fence after this tick's conversion. With all fences
        /// still pending (several ticks of backlog, already counted as
        /// late) the tick goes unmeasured.
        pub(crate) fn fence(&mut self, context: &ID3D11DeviceContext, now: Hns) {
            if let Some(fence) = self.fences.iter_mut().find(|f| f.issued.is_none()) {
                // SAFETY: our own query on the device's immediate context
                // (multithread-protected). The flush submits the conversion
                // now, so the fence measures the GPU, not our batching.
                unsafe {
                    context.End(&fence.query);
                    context.Flush();
                }
                fence.issued = Some(now);
            }
        }

        /// Records one tick after the first frame, and whether it was
        /// starved (late GPU work, or a frame dropped for want of a free
        /// surface or encoder input).
        pub(crate) fn tick(&mut self, starved: bool) {
            self.second.ticks += 1;
            self.second.starved += u32::from(starved);
        }

        /// Called once a second: the adaptive decision, if any, is applied.
        pub(crate) fn end_second(&mut self, now: Hns) {
            let second = std::mem::take(&mut self.second);
            if self.mode != GpuPriority::Adaptive {
                return;
            }
            match self.controller.observe(now, second) {
                Some(Decision::Raise(why)) => {
                    self.apply(now, HIGH, format!("{why}{}", self.scheduling))
                }
                Some(Decision::Lower(why)) => self.apply(now, NORMAL, why),
                None => {}
            }
        }

        /// Sets the device's priority. A refusal is reported, never fatal:
        /// recording at normal priority beats not recording.
        fn apply(&mut self, now: Hns, level: i32, reason: String) {
            // SAFETY: plain call on a valid device; -7..=7 is in range.
            let reason = match unsafe { self.dxgi.SetGPUThreadPriority(level) } {
                Ok(()) => {
                    self.level = level;
                    self.stats.gpu_priority.store(level, Ordering::Relaxed);
                    reason
                }
                Err(e) => format!("SetGPUThreadPriority({level}) failed ({e}); {reason}"),
            };
            self.stats.push_priority(GpuPriorityChange {
                time: now,
                level: self.level,
                reason,
            });
        }
    }
}
