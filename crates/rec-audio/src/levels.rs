//! A cheap loudness curve per audio track: one RMS value per 100 ms window,
//! kept for the replay-buffer length, so a saved clip can say where the
//! action is (shouting, gunfire, a quiet menu) without anyone decoding the
//! audio.
//!
//! This module is pure (no devices, no clock reads). A `Meter` sits in front
//! of each track's encoder, sums squares of the samples it is given and
//! pushes a [`Level`] into the shared [`Levels`] whenever a window completes.
//! It runs on the MMCSS audio thread: one pass over the samples that are
//! encoded anyway, and one short lock per window.
//!
//! # Scale
//!
//! The value is the window's RMS over both channels in dB, mapped linearly
//! from -60 dBFS (0) to 0 dBFS (255): 60 dB spans the range between a quiet
//! room and a clipping game mix, and one step is 0.24 dB, finer than anyone
//! hears. Digital silence is 0, and so is anything quieter than -60 dBFS.

// Only the Windows capture thread creates meters.
#![cfg_attr(not(windows), allow(dead_code))]

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rec_core::replay::TrackId;
use rec_core::time::{Hns, millis};

use crate::SAMPLE_RATE;
use crate::timeline::{Timeline, samples_to_hns};

/// Levels per second per track.
pub const LEVEL_HZ: u32 = 10;
/// Frames per window: 4800 at 48 kHz.
pub const LEVEL_WINDOW: u64 = (SAMPLE_RATE / LEVEL_HZ) as u64;

/// Duration of one window (exactly 100 ms at 48 kHz).
const WINDOW_HNS: Hns = samples_to_hns(LEVEL_WINDOW as i64);
/// Level of a window at or below this many dBFS is 0.
const FLOOR_DB: f64 = 60.0;
/// What an `i16` sample of full scale squares to.
const FULL_SCALE_SQUARED: f64 = 32_768.0 * 32_768.0;
/// How long levels are kept until [`Levels::set_history`] says otherwise: the
/// longest replay buffer plus some slack for the clip's own length.
const DEFAULT_KEEP: Hns = millis(20 * 60_000 + 30_000);

/// Loudness of one 100 ms window of one track.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Level {
    /// QPC time of the window's first sample.
    pub t: Hns,
    /// RMS loudness over both channels on a dB scale: 0 is -60 dBFS or quieter
    /// (including digital silence), 255 is 0 dBFS (full scale); linear in dB
    /// between.
    pub value: u8,
}

#[derive(Debug)]
struct Inner {
    keep: Hns,
    /// Per track, oldest first (window start times only ever increase).
    tracks: BTreeMap<TrackId, VecDeque<Level>>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            keep: DEFAULT_KEEP,
            tracks: BTreeMap::new(),
        }
    }
}

impl Inner {
    fn prune(keep: Hns, levels: &mut VecDeque<Level>) {
        let Some(newest) = levels.back().map(|l| l.t) else {
            return;
        };
        while levels.front().is_some_and(|l| newest - l.t > keep) {
            levels.pop_front();
        }
    }
}

/// Recent levels of every track, shared between the capture thread and
/// readers. Cloning shares the same levels.
#[derive(Clone, Debug, Default)]
pub struct Levels(Arc<Mutex<Inner>>);

impl Levels {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Keeps windows that started at most `keep` before the newest one (per
    /// track). Default 20 minutes + 30 s.
    pub fn set_history(&self, keep: Hns) {
        let mut inner = self.lock();
        inner.keep = keep;
        for levels in inner.tracks.values_mut() {
            Inner::prune(keep, levels);
        }
    }

    /// The windows of `track` overlapping `[from, to)`, oldest first.
    pub fn range(&self, track: TrackId, from: Hns, to: Hns) -> Vec<Level> {
        let inner = self.lock();
        let Some(levels) = inner.tracks.get(&track) else {
            return Vec::new();
        };
        let start = levels.partition_point(|l| l.t + WINDOW_HNS <= from);
        let end = levels.partition_point(|l| l.t < to).max(start);
        levels.range(start..end).copied().collect()
    }

    fn push(&self, track: TrackId, level: Level) {
        let mut inner = self.lock();
        let keep = inner.keep;
        let levels = inner.tracks.entry(track).or_default();
        levels.push_back(level);
        Inner::prune(keep, levels);
    }
}

/// Accumulates one track's samples into windows.
#[derive(Debug)]
pub(crate) struct Meter {
    track: TrackId,
    levels: Levels,
    /// Frames seen so far: the timeline index of the next one.
    index: i64,
    /// Frames in the open window.
    filled: u64,
    /// Sum of squares of the open window's samples.
    sum: u64,
    /// Time of the open window's first sample, read when it opened.
    start: Hns,
}

impl Meter {
    pub(crate) fn new(track: TrackId, levels: &Levels) -> Self {
        Self {
            track,
            levels: levels.clone(),
            index: 0,
            filled: 0,
            sum: 0,
            start: 0,
        }
    }

    /// Accounts for `pcm` (interleaved stereo), or `frames` frames of silence
    /// when `pcm` is `None`, which come next on `timeline`. A window's time is
    /// read when its first frame arrives, so it follows the timeline's
    /// placement of that data.
    pub(crate) fn push(&mut self, timeline: &Timeline, pcm: Option<&[i16]>, frames: u64) {
        let frames = pcm.map_or(frames, |pcm| (pcm.len() / 2) as u64);
        let mut done = 0;
        while done < frames {
            if self.filled == 0 {
                self.start = timeline.time_of(self.index);
            }
            let n = (LEVEL_WINDOW - self.filled).min(frames - done);
            if let Some(pcm) = pcm {
                self.sum += square_sum(&pcm[done as usize * 2..(done + n) as usize * 2]);
            }
            self.filled += n;
            self.index += n as i64;
            done += n;
            if self.filled == LEVEL_WINDOW {
                self.levels.push(
                    self.track,
                    Level {
                        t: self.start,
                        value: value(self.sum),
                    },
                );
                self.filled = 0;
                self.sum = 0;
            }
        }
    }
}

fn square_sum(pcm: &[i16]) -> u64 {
    pcm.iter()
        .map(|&s| {
            let a = u32::from(s.unsigned_abs());
            u64::from(a * a)
        })
        .sum()
}

/// The 0..=255 value of a window whose samples square-sum to `sum`.
fn value(sum: u64) -> u8 {
    if sum == 0 {
        return 0;
    }
    let mean = sum as f64 / (2 * LEVEL_WINDOW) as f64;
    let db = 10.0 * (mean / FULL_SCALE_SQUARED).log10();
    ((db + FLOOR_DB) / FLOOR_DB * 255.0)
        .round()
        .clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::TimelineConfig;

    const START: Hns = 1_000 * rec_core::time::HNS_PER_SEC;
    const WINDOW: usize = LEVEL_WINDOW as usize;

    fn timeline() -> Timeline {
        Timeline::new(START, TimelineConfig::default())
    }

    /// Interleaved stereo sine of `frames` frames whose RMS is `dbfs`.
    fn sine(dbfs: f64, frames: usize) -> Vec<i16> {
        let peak = 32_768.0 * 10f64.powf(dbfs / 20.0) * std::f64::consts::SQRT_2;
        (0..frames)
            .flat_map(|i| {
                let v = (peak * (i as f64 * 0.1).sin()).round() as i16;
                [v, v]
            })
            .collect()
    }

    /// One window of a constant, to get a known level.
    fn flat(amp: i16) -> Vec<i16> {
        vec![amp; WINDOW * 2]
    }

    fn measure(pcm: &[i16]) -> u8 {
        let levels = Levels::default();
        Meter::new(1, &levels).push(&timeline(), Some(pcm), (pcm.len() / 2) as u64);
        let got = levels.range(1, START, START + WINDOW_HNS);
        assert_eq!(got.len(), 1);
        got[0].value
    }

    #[test]
    fn window_is_a_tenth_of_a_second() {
        assert_eq!(LEVEL_WINDOW, 4_800);
        assert_eq!(WINDOW_HNS, 1_000_000);
    }

    #[test]
    fn db_mapping() {
        // Full-scale square wave.
        let square: Vec<i16> = (0..WINDOW * 2)
            .map(|i| if i / 2 % 2 == 0 { i16::MAX } else { i16::MIN })
            .collect();
        assert_eq!(measure(&square), 255);
        assert_eq!(measure(&flat(i16::MIN)), 255);
        assert_eq!(measure(&flat(0)), 0);
        // RMS -30 dBFS is halfway.
        assert!(measure(&sine(-30.0, WINDOW)).abs_diff(128) <= 2);
        assert!(measure(&sine(-6.0, WINDOW)).abs_diff(230) <= 2);
        // 60 dB floor: -66 dBFS and a single LSB are 0.
        assert_eq!(measure(&sine(-66.0, WINDOW)), 0);
        assert_eq!(measure(&flat(1)), 0);
        // Just above it is not 0.
        assert!(measure(&sine(-58.0, WINDOW)) > 0);
        // Louder is never lower.
        let steps: Vec<u8> = (0..=12)
            .map(|i| measure(&sine(-60.0 + f64::from(i) * 5.0, WINDOW)))
            .collect();
        assert!(steps.windows(2).all(|w| w[0] <= w[1]), "{steps:?}");
    }

    #[test]
    fn silence_counts_as_zeros() {
        let levels = Levels::default();
        let tl = timeline();
        let mut meter = Meter::new(1, &levels);
        let tone = sine(-20.0, WINDOW / 2);
        meter.push(&tl, Some(&tone), (WINDOW / 2) as u64);
        meter.push(&tl, None, (WINDOW / 2) as u64);
        // A full window of the same tone, for reference.
        meter.push(&tl, Some(&sine(-20.0, WINDOW)), LEVEL_WINDOW);
        let got = levels.range(1, START, START + 2 * WINDOW_HNS);
        assert_eq!(got.len(), 2);
        // Half the energy is 3 dB, 12.75 steps, lower.
        let drop = i32::from(got[1].value) - i32::from(got[0].value);
        assert!((12..=14).contains(&drop), "{got:?}");
        // A window of pure silence is 0.
        meter.push(&tl, None, LEVEL_WINDOW);
        assert_eq!(levels.range(1, START, START + 3 * WINDOW_HNS)[2].value, 0);
    }

    #[test]
    fn windows_follow_the_timeline() {
        // Window k has amplitude 100 * (k + 1).
        let windows = 12;
        let signal: Vec<i16> = (0..windows)
            .flat_map(|k| flat(100 * (k as i16 + 1)))
            .collect();
        let whole = Levels::default();
        Meter::new(1, &whole).push(&timeline(), Some(&signal), (signal.len() / 2) as u64);
        let expected = whole.range(1, START, START + 100 * WINDOW_HNS);
        assert_eq!(expected.len(), windows);
        for (k, level) in expected.iter().enumerate() {
            assert_eq!(level.t, START + k as i64 * WINDOW_HNS);
        }
        assert!(expected.windows(2).all(|w| w[0].value < w[1].value));

        // The same signal in odd chunks, some spanning several windows and
        // some ending exactly on a boundary, with silence-free pcm only.
        let chunks = [1, 777, 4_800, 4_801, 9_599, 3, 12_345, 4_800, 1];
        let levels = Levels::default();
        let tl = timeline();
        let mut meter = Meter::new(1, &levels);
        let mut at = 0;
        let mut i = 0;
        while at < signal.len() / 2 {
            let n = chunks[i % chunks.len()].min(signal.len() / 2 - at);
            meter.push(&tl, Some(&signal[at * 2..(at + n) * 2]), n as u64);
            at += n;
            i += 1;
        }
        assert_eq!(levels.range(1, START, START + 100 * WINDOW_HNS), expected);
    }

    #[test]
    fn mixed_silence_and_audio_chunks_keep_alignment() {
        let levels = Levels::default();
        let tl = timeline();
        let mut meter = Meter::new(1, &levels);
        // 1.5 windows of silence, then loud audio from frame 7200 on: the
        // second window is half silent, the third full.
        meter.push(&tl, None, 7_200);
        let loud = flat(i16::MAX);
        meter.push(&tl, Some(&loud[..2 * 2_400]), 2_400);
        meter.push(&tl, Some(&loud), LEVEL_WINDOW);
        let got = levels.range(1, START, START + 10 * WINDOW_HNS);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].value, 0);
        assert!((1..255).contains(&got[1].value));
        assert_eq!(got[2].value, 255);
        assert_eq!(got[2].t, START + 2 * WINDOW_HNS);
    }

    #[test]
    fn windows_are_timed_through_placement_and_long_gaps() {
        let levels = Levels::default();
        let mut tl = timeline();
        let mut meter = Meter::new(1, &levels);
        let tone = vec![1000i16; 3 * WINDOW * 2];
        let mut feed = |tl: &mut Timeline, t: Hns, frames: usize| {
            let place = tl.on_data(Some(t), frames as u32, t);
            meter.push(tl, None, place.silence_before);
            meter.push(tl, Some(&tone[..frames * 2]), frames as u64);
        };
        // The first packet lands 50 ms after START: the silence in front of
        // it is part of the first window, which therefore starts at START.
        feed(&mut tl, START + millis(50), WINDOW);
        // After a gap of a minute the mapping jumps inside the filled silence.
        tl.mark_discontinuity();
        feed(&mut tl, START + millis(60_000), 3 * WINDOW);

        let got = levels.range(1, i64::MIN, i64::MAX);
        assert!(got.len() > 8, "{}", got.len());
        assert_eq!(got[0].t, START);
        for (k, level) in got.iter().enumerate() {
            assert_eq!(level.t, tl.time_of((k * WINDOW) as i64), "window {k}");
        }
        assert!(got.windows(2).all(|w| w[0].t < w[1].t));
        // The last windows are the data, 60 s in.
        assert!(got.last().is_some_and(|l| l.t > START + millis(59_000)));
    }

    fn filled(levels: &Levels, track: TrackId, windows: i64) {
        for k in 0..windows {
            levels.push(
                track,
                Level {
                    t: START + k * WINDOW_HNS,
                    value: k as u8,
                },
            );
        }
    }

    fn values(levels: Vec<Level>) -> Vec<u8> {
        levels.into_iter().map(|l| l.value).collect()
    }

    #[test]
    fn range_boundaries() {
        let levels = Levels::default();
        filled(&levels, 3, 10);
        let at = |k: i64| START + k * WINDOW_HNS;
        // A window ending exactly at `from` does not overlap; one starting
        // exactly at `to` does not either.
        assert_eq!(values(levels.range(3, at(1), at(3))), [1, 2]);
        // Mid-window bounds include the windows they fall in.
        assert_eq!(values(levels.range(3, at(1) + 1, at(3) - 1)), [1, 2],);
        assert_eq!(values(levels.range(3, at(1) - 1, at(3) + 1)), [0, 1, 2, 3],);
        // Wider than the data, and a single instant.
        assert_eq!(levels.range(3, i64::MIN, i64::MAX).len(), 10);
        assert_eq!(values(levels.range(3, at(4) + 5, at(4) + 6)), [4]);
        // Empty and inverted ranges, other tracks, before and after the data.
        assert!(levels.range(3, at(2), at(2)).is_empty());
        assert!(levels.range(3, at(5), at(2)).is_empty());
        assert!(levels.range(4, i64::MIN, i64::MAX).is_empty());
        assert!(levels.range(3, at(-5), at(0)).is_empty());
        assert!(levels.range(3, at(10), at(15)).is_empty());
    }

    #[test]
    fn history_prunes_per_track() {
        let levels = Levels::default();
        levels.set_history(3 * WINDOW_HNS);
        filled(&levels, 1, 10);
        filled(&levels, 2, 5);
        // Windows up to `keep` before the newest stay: 6, 7, 8, 9.
        assert_eq!(values(levels.range(1, i64::MIN, i64::MAX)), [6, 7, 8, 9]);
        assert_eq!(values(levels.range(2, i64::MIN, i64::MAX)), [1, 2, 3, 4]);
        // Shrinking applies at once; growing does not bring windows back.
        levels.set_history(WINDOW_HNS);
        assert_eq!(values(levels.range(1, i64::MIN, i64::MAX)), [8, 9]);
        levels.set_history(100 * WINDOW_HNS);
        assert_eq!(values(levels.range(1, i64::MIN, i64::MAX)), [8, 9]);
    }

    #[test]
    fn default_history_covers_the_replay_buffer() {
        let levels = Levels::default();
        // 21 minutes of windows.
        filled(&levels, 1, 21 * 60 * 10);
        let kept = levels.range(1, i64::MIN, i64::MAX);
        assert_eq!(kept.len(), 20 * 60 * 10 + 30 * 10 + 1);
        assert_eq!(
            kept.last().map(|l| l.t),
            Some(START + (21 * 600 - 1) * WINDOW_HNS)
        );
    }

    #[test]
    fn clones_share_levels() {
        let levels = Levels::default();
        let reader = levels.clone();
        let mut meter = Meter::new(7, &levels);
        meter.push(&timeline(), Some(&flat(i16::MAX)), LEVEL_WINDOW);
        assert_eq!(values(reader.range(7, i64::MIN, i64::MAX)), [255]);
    }
}
