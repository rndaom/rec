//! Maps captured audio onto a continuous sample timeline on the QPC clock.
//!
//! This module is pure (no devices, no clock reads) so every rule below is
//! unit-tested.
//!
//! The encoder is fed one gap-free stream of 48 kHz samples. Sample `i` of that
//! stream plays at [`Timeline::time_of`]`(i)`. The mapping is linear and only
//! moves during a very long gap (see *Long gaps*), so packet timestamps are
//! derived from sample counts and never jitter.
//!
//! The capture side tells the timeline what arrived and when; the timeline
//! answers how much silence to insert first and how many frames to drop:
//!
//! * **Anchoring.** The first packet, and the first packet after a
//!   discontinuity (device restart, `DATA_DISCONTINUITY`, a device-position
//!   jump, or a silence fill), is placed by its QPC timestamp. A gap before it
//!   becomes silence. An overlap (it starts before the end of what was already
//!   fed) drops the overlapping frames, except that overlaps up to
//!   [`TimelineConfig::overlap_tolerance`] are absorbed by appending, so a few
//!   milliseconds of jitter never cut a transient.
//! * **Continuous data** is appended by sample count. Its timestamp only feeds
//!   a smoothed drift estimate between the device clock and QPC. When the
//!   estimate passes [`TimelineConfig::drift_threshold`], silence is inserted
//!   (device clock slow) or frames are dropped (device clock fast) to pull the
//!   timeline back onto QPC.
//! * **Silence fill.** Loopback delivers nothing while nothing plays and a
//!   removed device delivers nothing at all, but the track must stay
//!   continuous. [`Timeline::on_idle`] fills silence up to
//!   `now - idle_margin`. The margin is larger than the normal delivery delay,
//!   so data that is merely in flight is never pre-empted by silence. Data that
//!   arrives for an already filled range is dropped, never double-counted.
//! * **Long gaps.** A gap longer than [`TimelineConfig::max_fill`] (system
//!   sleep, a suspended process) is not encoded sample by sample. One second of
//!   silence is fed and the mapping jumps forward in its middle, so the jump
//!   lies inside silence on both sides of the encoder delay. Packet timestamps
//!   then have a hole there, which the muxer treats like any other gap.

use rec_core::time::{HNS_PER_SEC, Hns, millis};

/// Output sample rate of the whole pipeline.
pub const SAMPLE_RATE: u32 = 48_000;

const RATE: i64 = SAMPLE_RATE as i64;

/// Duration of `samples` at [`SAMPLE_RATE`], rounded down. Exact for any
/// count (no overflow for centuries of audio), and consistent for negative
/// counts, so `samples_to_hns(a + b) - samples_to_hns(a)` never accumulates
/// rounding error.
pub const fn samples_to_hns(samples: i64) -> Hns {
    let whole = samples.div_euclid(RATE);
    let rest = samples.rem_euclid(RATE);
    whole * HNS_PER_SEC + rest * HNS_PER_SEC / RATE
}

/// Number of samples closest to `t` at [`SAMPLE_RATE`]. `t >= 0`.
pub const fn hns_to_samples(t: Hns) -> u64 {
    let whole = t / HNS_PER_SEC;
    let rest = t % HNS_PER_SEC;
    (whole * RATE + (rest * RATE + HNS_PER_SEC / 2) / HNS_PER_SEC) as u64
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimelineConfig {
    /// Silence is only filled up to `now - idle_margin`. Must exceed the
    /// normal delay between a frame's timestamp and our reading it: one poll
    /// period (up to ~47 ms) plus one engine period (10 ms). Loopback
    /// timestamps run 10-20 ms *ahead* of reading (measured), which only adds
    /// slack; microphone timestamps are in the past.
    pub idle_margin: Hns,
    /// Anchoring overlaps up to this size are absorbed by appending.
    pub overlap_tolerance: Hns,
    /// Smoothed drift beyond this is corrected.
    pub drift_threshold: Hns,
    /// Gaps longer than this jump the mapping instead of being encoded.
    /// Must be longer than [`LONG_GAP_FILL`].
    pub max_fill: Hns,
}

impl Default for TimelineConfig {
    fn default() -> Self {
        Self {
            idle_margin: millis(100),
            overlap_tolerance: millis(5),
            drift_threshold: millis(20),
            max_fill: millis(10_000),
        }
    }
}

/// Silence fed for a long gap; the mapping jumps halfway through it.
pub const LONG_GAP_FILL: u64 = SAMPLE_RATE as u64;

/// Weight of a new drift observation is `1 / DRIFT_SMOOTHING` (a time constant
/// of about 16 packets, or 160 ms at the usual 10 ms engine period).
const DRIFT_SMOOTHING: i64 = 16;

/// Where one captured packet goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Placement {
    /// Silence samples to feed before the packet.
    pub silence_before: u64,
    /// Frames to drop from the front of the packet; feed `frames[skip..]`.
    pub skip: u32,
}

/// Running totals, in samples.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimelineStats {
    pub silence_samples: u64,
    pub dropped_samples: u64,
    pub drift_corrections: u32,
    pub long_gaps: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Anchor {
    index: i64,
    time: Hns,
}

#[derive(Clone, Debug)]
pub struct Timeline {
    cfg: TimelineConfig,
    /// Maps samples at or after `anchor.index`.
    anchor: Anchor,
    /// Maps samples before `anchor.index` (only differs after a long gap).
    prev_anchor: Anchor,
    /// Samples fed to the encoder so far; the index of the next sample.
    written: i64,
    need_anchor: bool,
    /// Frames still to drop from upcoming data.
    pending_skip: u64,
    /// Smoothed `(capture time - timeline time)` of continuous data.
    drift: Hns,
    stats: TimelineStats,
}

impl Timeline {
    /// A timeline whose sample 0 is at `start` (QPC hns).
    pub fn new(start: Hns, cfg: TimelineConfig) -> Self {
        debug_assert!(cfg.max_fill > samples_to_hns(LONG_GAP_FILL as i64));
        let anchor = Anchor {
            index: 0,
            time: start,
        };
        Self {
            cfg,
            anchor,
            prev_anchor: anchor,
            written: 0,
            need_anchor: true,
            pending_skip: 0,
            drift: 0,
            stats: TimelineStats::default(),
        }
    }

    /// Time at which output sample `index` plays. Negative indices are
    /// allowed (encoder priming).
    pub fn time_of(&self, index: i64) -> Hns {
        let a = self.anchor_for(index);
        a.time + samples_to_hns(index - a.index)
    }

    /// Duration of the `len` samples starting at `index`, measured on the
    /// mapping that times `index` (so a packet straddling a long-gap jump keeps
    /// its nominal duration).
    pub fn duration_of(&self, index: i64, len: i64) -> Hns {
        let a = self.anchor_for(index);
        samples_to_hns(index + len - a.index) - samples_to_hns(index - a.index)
    }

    fn anchor_for(&self, index: i64) -> Anchor {
        if index >= self.anchor.index {
            self.anchor
        } else {
            self.prev_anchor
        }
    }

    /// Time of the next sample to be fed.
    pub fn position(&self) -> Hns {
        self.time_of(self.written)
    }

    /// Samples fed so far.
    pub fn written(&self) -> u64 {
        self.written as u64
    }

    pub fn stats(&self) -> TimelineStats {
        self.stats
    }

    /// The next packet does not continue the previous one (new device,
    /// `DATA_DISCONTINUITY`, device position jump): place it by its timestamp.
    pub fn mark_discontinuity(&mut self) {
        self.need_anchor = true;
        self.pending_skip = 0;
    }

    /// Places a captured packet of `frames` frames whose first frame was
    /// captured at `t` (QPC hns; `None` if the device flagged a timestamp
    /// error). `now` is the current QPC time, used only to estimate a missing
    /// timestamp.
    pub fn on_data(&mut self, t: Option<Hns>, frames: u32, now: Hns) -> Placement {
        let pos = self.position();
        let mut silence = 0;
        if self.need_anchor {
            self.need_anchor = false;
            self.pending_skip = 0;
            self.drift = 0;
            // Without a timestamp, assume the last frame was captured just now.
            let t = t.unwrap_or_else(|| now - samples_to_hns(frames.into()));
            let delta = t - pos;
            if delta > 0 {
                silence = self.fill_to(t);
            } else if delta < -self.cfg.overlap_tolerance {
                self.pending_skip = hns_to_samples(-delta);
            } else {
                // Small overlap: append; the packet lands up to the tolerance
                // late, which the drift estimate remembers.
                self.drift = delta;
            }
        } else if let Some(t) = t {
            // Capture time of the next frame that will be kept (frames still
            // pending a drop are skipped) vs. where it lands on the timeline.
            let kept_start = t + samples_to_hns(self.pending_skip as i64);
            let err = kept_start - pos;
            self.drift += (err - self.drift) / DRIFT_SMOOTHING;
            if self.drift > self.cfg.drift_threshold {
                // Device clock slower than QPC: the timeline fell behind.
                silence = self.fill_to(pos + self.drift);
                self.stats.drift_corrections += 1;
                self.drift = 0;
            } else if self.drift < -self.cfg.drift_threshold {
                // Device clock faster than QPC: the timeline ran ahead.
                self.pending_skip += hns_to_samples(-self.drift);
                self.stats.drift_corrections += 1;
                self.drift = 0;
            }
        }
        let skip = self.pending_skip.min(frames.into());
        self.pending_skip -= skip;
        self.stats.dropped_samples += skip;
        self.written += (u64::from(frames) - skip) as i64;
        Placement {
            silence_before: silence,
            skip: skip as u32,
        }
    }

    /// No data is pending: fill silence up to `now - idle_margin`. Returns the
    /// silence samples to feed.
    pub fn on_idle(&mut self, now: Hns) -> u64 {
        self.fill_idle_to(now - self.cfg.idle_margin)
    }

    /// Capture is stopping: fill silence right up to `now`.
    pub fn on_finish(&mut self, now: Hns) -> u64 {
        self.fill_idle_to(now)
    }

    fn fill_idle_to(&mut self, target: Hns) -> u64 {
        if target - self.position() < samples_to_hns(1) {
            return 0;
        }
        let n = self.fill_to(target);
        // Whatever arrives next must be placed by its timestamp so it neither
        // overlaps the silence nor leaves a hole.
        self.need_anchor = true;
        self.pending_skip = 0;
        n
    }

    /// Feeds silence from the current position up to `target` and returns the
    /// sample count. Long gaps jump the mapping instead.
    fn fill_to(&mut self, target: Hns) -> u64 {
        let gap = target - self.position();
        if gap <= 0 {
            return 0;
        }
        let n = if gap > self.cfg.max_fill {
            let half = (LONG_GAP_FILL / 2) as i64;
            self.prev_anchor = self.anchor;
            self.anchor = Anchor {
                index: self.written + half,
                time: target - samples_to_hns(half),
            };
            self.stats.long_gaps += 1;
            LONG_GAP_FILL
        } else {
            hns_to_samples(gap)
        };
        self.written += n as i64;
        self.stats.silence_samples += n;
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: Hns = 1_000 * HNS_PER_SEC;

    fn ms(n: i64) -> Hns {
        millis(n)
    }

    fn samples(ms: i64) -> u32 {
        (ms * RATE / 1000) as u32
    }

    fn tl() -> Timeline {
        Timeline::new(START, TimelineConfig::default())
    }

    #[test]
    fn sample_conversions_are_exact() {
        assert_eq!(samples_to_hns(48_000), HNS_PER_SEC);
        assert_eq!(samples_to_hns(1024), 213_333);
        assert_eq!(samples_to_hns(2048), 426_666);
        assert_eq!(samples_to_hns(-1024), -213_334);
        assert_eq!(hns_to_samples(HNS_PER_SEC), 48_000);
        assert_eq!(hns_to_samples(213_333), 1024);
        assert_eq!(hns_to_samples(ms(10)), 480);
        // A year of samples does not overflow.
        let year = 365 * 86_400 * RATE;
        assert_eq!(samples_to_hns(year), 365 * 86_400 * HNS_PER_SEC);
        // Durations of consecutive 1024-sample packets add up exactly.
        let total: Hns = (0..48)
            .map(|i| samples_to_hns((i + 1) * 1024) - samples_to_hns(i * 1024))
            .sum();
        assert_eq!(total, samples_to_hns(48 * 1024));
    }

    #[test]
    fn first_packet_is_placed_by_timestamp() {
        let mut t = tl();
        let p = t.on_data(Some(START + ms(15)), 480, START + ms(30));
        assert_eq!(p.silence_before, 720);
        assert_eq!(p.skip, 0);
        assert_eq!(t.written(), 1200);
        assert_eq!(t.position(), START + ms(25));
    }

    #[test]
    fn data_from_before_start_is_dropped() {
        let mut t = tl();
        let p = t.on_data(Some(START - ms(20)), 1440, START + ms(10));
        assert_eq!(
            p,
            Placement {
                silence_before: 0,
                skip: 960
            }
        );
        assert_eq!(t.position(), START + ms(10));
        assert_eq!(t.stats().dropped_samples, 960);
    }

    #[test]
    fn continuous_data_appends_by_count_and_ignores_jitter() {
        let mut t = tl();
        t.on_data(Some(START), 480, START);
        for i in 1..1000 {
            // +-3 ms of timestamp jitter.
            let jitter = ms([0, 3, -3, 1][i % 4]);
            let p = t.on_data(Some(START + ms(10 * i as i64) + jitter), 480, 0);
            assert_eq!(p, Placement::default());
        }
        assert_eq!(t.written(), 480_000);
        assert_eq!(t.position(), START + ms(10_000));
        assert_eq!(t.stats().drift_corrections, 0);
    }

    #[test]
    fn slow_device_clock_inserts_silence() {
        let mut t = tl();
        // The device delivers 480 frames per 10.001 ms of QPC time (100 ppm
        // slow): after 200 s the timeline is 20 ms behind.
        let mut inserted = 0;
        for i in 0..30_000_i64 {
            let ts = START + i * 100_010;
            inserted += t.on_data(Some(ts), 480, ts).silence_before;
        }
        let s = t.stats();
        assert_eq!(s.drift_corrections, 1);
        assert!(inserted > 900 && inserted < 1100, "inserted {inserted}");
        // Afterwards the timeline is back within the threshold of QPC.
        let end = START + 30_000 * 100_010;
        assert!((t.position() - end).abs() < ms(20));
    }

    #[test]
    fn fast_device_clock_drops_frames() {
        let mut t = tl();
        let mut dropped = 0;
        for i in 0..30_000_i64 {
            let ts = START + i * 99_990;
            dropped += u64::from(t.on_data(Some(ts), 480, ts).skip);
        }
        assert_eq!(t.stats().drift_corrections, 1);
        assert!(dropped > 900 && dropped < 1100, "dropped {dropped}");
        assert_eq!(t.stats().dropped_samples, dropped);
        let end = START + 30_000 * 99_990;
        assert!((t.position() - end).abs() < ms(20));
    }

    #[test]
    fn idle_fills_silence_up_to_the_margin() {
        let mut t = tl();
        let margin = TimelineConfig::default().idle_margin;
        assert_eq!(t.on_idle(START + margin), 0);
        assert_eq!(t.on_idle(START + margin + ms(100)), 4800);
        assert_eq!(t.on_idle(START + margin + ms(100)), 0);
        assert_eq!(t.on_idle(START + margin + ms(120)), 960);
        assert_eq!(t.position(), START + ms(120));
        assert_eq!(t.stats().silence_samples, 5760);
    }

    #[test]
    fn data_resuming_after_silence_lands_at_its_timestamp() {
        let mut t = tl();
        t.on_data(Some(START), 480, START);
        // Nothing plays for a while: silence up to START + 500 ms.
        t.on_idle(START + ms(600));
        assert_eq!(t.position(), START + ms(500));
        // Sound resumes at START + 530 ms.
        let p = t.on_data(Some(START + ms(530)), 480, START + ms(545));
        assert_eq!(
            p,
            Placement {
                silence_before: 1440,
                skip: 0
            }
        );
        assert_eq!(t.position(), START + ms(540));
        // ...and continues by count.
        let p = t.on_data(Some(START + ms(540)), 480, START + ms(555));
        assert_eq!(p, Placement::default());
    }

    #[test]
    fn late_data_for_a_filled_range_is_not_double_counted() {
        let mut t = tl();
        t.on_data(Some(START), 480, START);
        // The device stalls; silence is filled up to START + 200 ms.
        t.on_idle(START + ms(300));
        // Then the backlog arrives with its original timestamps.
        let mut kept = 0;
        for i in 1..30 {
            let p = t.on_data(Some(START + ms(10 * i)), 480, START + ms(290));
            assert_eq!(p.silence_before, 0);
            kept += 480 - p.skip;
        }
        // Frames for 10..200 ms were dropped; 200..300 ms were kept.
        assert_eq!(kept, 4800);
        assert_eq!(t.position(), START + ms(300));
        assert_eq!(t.stats().dropped_samples, 190 * 48);
    }

    #[test]
    fn small_overlap_is_absorbed() {
        let mut t = tl();
        t.on_data(Some(START), 480, START);
        t.on_idle(START + ms(200)); // filled to 100 ms
        // Resumes 3 ms "inside" the filled range: appended, not cut.
        let p = t.on_data(Some(START + ms(97)), 480, START + ms(110));
        assert_eq!(p, Placement::default());
        assert_eq!(t.position(), START + ms(110));
    }

    #[test]
    fn discontinuity_reanchors() {
        let mut t = tl();
        t.on_data(Some(START), 480, START);
        // A device restart: the new device's first packet is 50 ms later.
        t.mark_discontinuity();
        let p = t.on_data(Some(START + ms(60)), 480, START + ms(70));
        assert_eq!(p.silence_before, u64::from(samples(50)));
        assert_eq!(t.position(), START + ms(70));
    }

    #[test]
    fn missing_timestamp_is_estimated_from_now() {
        let mut t = tl();
        let p = t.on_data(None, 480, START + ms(40));
        assert_eq!(p.silence_before, u64::from(samples(30)));
        // Continuous packets without timestamps just append.
        let p = t.on_data(None, 480, START + ms(50));
        assert_eq!(p, Placement::default());
        assert_eq!(t.position(), START + ms(50));
    }

    #[test]
    fn long_gap_jumps_inside_silence() {
        let mut t = tl();
        t.on_data(Some(START), 480, START);
        let w0 = t.written() as i64;
        let now = START + 3_600 * HNS_PER_SEC; // an hour asleep
        let n = t.on_idle(now);
        assert_eq!(n, LONG_GAP_FILL);
        let s = t.stats();
        assert_eq!(s.long_gaps, 1);
        // Ends exactly at the fill target.
        assert_eq!(t.position(), now - TimelineConfig::default().idle_margin);
        // Samples before the jump keep the old mapping, so encoder output that
        // is still in flight is timed correctly...
        let before = w0 + 1000;
        assert_eq!(t.time_of(before), START + samples_to_hns(before));
        // ...and nominal durations hold on both sides of the jump.
        let jump = w0 + (LONG_GAP_FILL / 2) as i64;
        assert_eq!(t.duration_of(jump - 512, 1024), 213_333);
        assert!(t.time_of(jump) - t.time_of(jump - 1) > 3_000 * HNS_PER_SEC);
        // Normal operation resumes afterwards.
        let p = t.on_data(Some(now - ms(70)), 480, now);
        assert_eq!(p.silence_before, u64::from(samples(30)));
    }

    #[test]
    fn finish_fills_to_now() {
        let mut t = tl();
        t.on_data(Some(START), 480, START);
        assert_eq!(t.on_finish(START + ms(50)), u64::from(samples(40)));
        assert_eq!(t.position(), START + ms(50));
    }

    #[test]
    fn priming_indices_map_before_start() {
        let t = tl();
        assert_eq!(t.time_of(-1024), START - 213_334);
        assert_eq!(t.duration_of(-1024, 1024), 213_334);
    }
}
