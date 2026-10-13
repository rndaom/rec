//! Frame-time statistics. Pure functions, no I/O, so the definitions are
//! pinned down by unit tests.
//!
//! Definitions (the same as CapFrameX's "average of the slowest N%" style):
//!
//! * **Average FPS** = `frames / sum(frame times)`. This is the true mean rate
//!   over the capture, not the mean of per-frame FPS values (which would
//!   overweight fast frames).
//! * **1% low** / **0.1% low** FPS = `1000 / mean(slowest 1% (0.1%) of frame
//!   times in ms)`. The slowest `ceil(n * fraction)` frames are used, at least
//!   one.
//! * **Median frame time**: middle value (mean of the two middle values for an
//!   even count).
//! * **P99 frame time**: nearest-rank 99th percentile, so it is always a frame
//!   time that was actually observed.
//! * **Stutters**: frames slower than [`STUTTER_FACTOR`] times the median.

use rec_core::time::{HNS_PER_MS, Hns};
use serde::Serialize;

/// A frame is a "stutter" when it takes longer than this many medians.
pub const STUTTER_FACTOR: f64 = 2.5;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FrameStats {
    pub frames: usize,
    /// Sum of the frame times, in seconds.
    pub duration_s: f64,
    pub avg_fps: f64,
    pub low_1_fps: f64,
    pub low_01_fps: f64,
    pub median_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub stutters: usize,
}

/// Computes all statistics for a list of frame times in milliseconds, or
/// `None` if there are no frames. Frame times must be finite and positive; the
/// CSV parser guarantees that.
pub fn compute(frame_ms: &[f64]) -> Option<FrameStats> {
    if frame_ms.is_empty() {
        return None;
    }
    let mut sorted = frame_ms.to_vec();
    sorted.sort_by(f64::total_cmp);

    let total_ms: f64 = frame_ms.iter().sum();
    let median_ms = median_sorted(&sorted);
    let threshold = median_ms * STUTTER_FACTOR;
    Some(FrameStats {
        frames: frame_ms.len(),
        duration_s: total_ms / 1000.0,
        avg_fps: 1000.0 * frame_ms.len() as f64 / total_ms,
        low_1_fps: low_fps_sorted(&sorted, 0.01),
        low_01_fps: low_fps_sorted(&sorted, 0.001),
        median_ms,
        p99_ms: percentile_sorted(&sorted, 99.0),
        max_ms: sorted[sorted.len() - 1],
        stutters: frame_ms.iter().filter(|&&t| t > threshold).count(),
    })
}

/// Median of an ascending, non-empty slice.
pub fn median_sorted(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

/// Nearest-rank percentile (`p` in 0..=100) of an ascending, non-empty slice.
pub fn percentile_sorted(sorted: &[f64], p: f64) -> f64 {
    let rank = ((p / 100.0) * sorted.len() as f64 - 1e-9).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// FPS equivalent of the mean of the slowest `fraction` of frames in an
/// ascending, non-empty slice.
pub fn low_fps_sorted(sorted: &[f64], fraction: f64) -> f64 {
    // The epsilon keeps `n * fraction` landing just above an integer (floating
    // point error) from pulling in one extra frame.
    let count = ((sorted.len() as f64 * fraction - 1e-9).ceil() as usize).clamp(1, sorted.len());
    let slowest = &sorted[sorted.len() - count..];
    1000.0 / mean(slowest)
}

pub fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len() as f64
}

/// Sample standard deviation; 0 for fewer than two values.
pub fn std_dev(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let m = mean(values);
    let var = values.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (values.len() - 1) as f64;
    var.sqrt()
}

/// The worst frame time around one clip save.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SaveSpike {
    /// Seconds from the first captured frame to the save request.
    pub at_s: f64,
    pub worst_ms: f64,
    /// `worst_ms` divided by the phase's median frame time.
    pub ratio: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SaveAnalysis {
    pub spikes: Vec<SaveSpike>,
    /// Saves whose window was not fully inside the captured frames.
    pub skipped: usize,
}

/// Finds the worst frame time around each save.
///
/// `saves` holds `(requested, completed)` on the same QPC clock as
/// `timestamps`. The window is one second before the request to one second
/// after completion, which is exactly "±1 s of the request" for a save that
/// completes at once (a remux of a few seconds of video does) and still covers
/// a slow save. Windows that run past either end of the capture are skipped:
/// a partial window would understate the spike.
pub fn save_spikes(
    timestamps: &[Hns],
    frame_ms: &[f64],
    saves: &[(Hns, Hns)],
    median_ms: f64,
) -> SaveAnalysis {
    const MARGIN: Hns = 1000 * HNS_PER_MS;
    let mut out = SaveAnalysis::default();
    let (Some(&first), Some(&last)) = (timestamps.first(), timestamps.last()) else {
        out.skipped = saves.len();
        return out;
    };
    for &(requested, completed) in saves {
        let from = requested - MARGIN;
        let to = requested.max(completed) + MARGIN;
        if from < first || to > last {
            out.skipped += 1;
            continue;
        }
        let worst = timestamps
            .iter()
            .zip(frame_ms)
            .filter(|&(&t, _)| (from..=to).contains(&t))
            .map(|(_, &ms)| ms)
            .fold(f64::NEG_INFINITY, f64::max);
        if worst.is_finite() {
            out.spikes.push(SaveSpike {
                at_s: (requested - first) as f64 / 1e7,
                worst_ms: worst,
                ratio: worst / median_ms,
            });
        } else {
            out.skipped += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn empty_input_has_no_stats() {
        assert!(compute(&[]).is_none());
    }

    #[test]
    fn constant_frame_times() {
        let s = compute(&[10.0; 200]).unwrap();
        assert_eq!(s.frames, 200);
        approx(s.duration_s, 2.0);
        approx(s.avg_fps, 100.0);
        approx(s.low_1_fps, 100.0);
        approx(s.low_01_fps, 100.0);
        approx(s.median_ms, 10.0);
        approx(s.p99_ms, 10.0);
        approx(s.max_ms, 10.0);
        assert_eq!(s.stutters, 0);
    }

    #[test]
    fn percentiles_on_known_data() {
        // 1..=100 ms, shuffled so sorting matters.
        let mut data: Vec<f64> = (1..=100).map(f64::from).collect();
        data.reverse();
        data.swap(3, 77);
        let s = compute(&data).unwrap();
        approx(s.median_ms, 50.5);
        approx(s.p99_ms, 99.0);
        approx(s.max_ms, 100.0);
        // Slowest 1% of 100 frames is the single 100 ms frame.
        approx(s.low_1_fps, 10.0);
        approx(s.low_01_fps, 10.0);
        // Average FPS is frames / total time: 100 / 5.05 s.
        approx(s.avg_fps, 100.0 / 5.05);
    }

    #[test]
    fn lows_average_the_slowest_frames() {
        // 1000 frames: 990 at 10 ms, nine at 40 ms, one at 100 ms.
        let mut data = vec![10.0; 990];
        data.extend([40.0; 9]);
        data.push(100.0);
        let s = compute(&data).unwrap();
        // Slowest 1% = 10 frames: (9 * 40 + 100) / 10 = 46 ms.
        approx(s.low_1_fps, 1000.0 / 46.0);
        // Slowest 0.1% = 1 frame.
        approx(s.low_01_fps, 10.0);
        approx(s.p99_ms, 10.0);
        // 40 ms and 100 ms are both above 2.5 x 10 ms.
        assert_eq!(s.stutters, 10);
    }

    #[test]
    fn lows_use_at_least_one_frame_and_round_up() {
        let s = compute(&[10.0, 20.0, 30.0]).unwrap();
        approx(s.low_1_fps, 1000.0 / 30.0);
        // 250 frames: 1% is 2.5, rounded up to 3 frames.
        let mut data = vec![10.0; 247];
        data.extend([20.0, 30.0, 40.0]);
        let s = compute(&data).unwrap();
        approx(s.low_1_fps, 1000.0 / 30.0);
    }

    #[test]
    fn median_of_even_count_averages_the_middle() {
        approx(median_sorted(&[1.0, 2.0, 3.0, 10.0]), 2.5);
        approx(median_sorted(&[7.0]), 7.0);
    }

    #[test]
    fn std_dev_matches_hand_calculation() {
        approx(
            std_dev(&[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]),
            2.138089935299395,
        );
        approx(std_dev(&[3.0]), 0.0);
    }

    #[test]
    fn save_spike_is_the_worst_frame_inside_the_window() {
        // A frame every 10 ms for 10 s, with a 50 ms hitch at t = 5.2 s.
        let step = 10 * HNS_PER_MS;
        let base: Hns = 1_000_000_000;
        let timestamps: Vec<Hns> = (0..1000).map(|i| base + i * step).collect();
        let mut frame_ms = vec![10.0; 1000];
        frame_ms[520] = 50.0;
        // Save requested at t = 5.0 s, completed 100 ms later: the hitch at
        // 5.2 s is inside [4.0, 6.1].
        let saves = [(base + 5_000 * HNS_PER_MS, base + 5_100 * HNS_PER_MS)];
        let a = save_spikes(&timestamps, &frame_ms, &saves, 10.0);
        assert_eq!(a.skipped, 0);
        assert_eq!(a.spikes.len(), 1);
        approx(a.spikes[0].worst_ms, 50.0);
        approx(a.spikes[0].ratio, 5.0);
        approx(a.spikes[0].at_s, 5.0);

        // A save at 8.0 s does not see it.
        let saves = [(base + 8_000 * HNS_PER_MS, base + 8_000 * HNS_PER_MS)];
        let a = save_spikes(&timestamps, &frame_ms, &saves, 10.0);
        approx(a.spikes[0].worst_ms, 10.0);
    }

    #[test]
    fn save_windows_that_run_off_the_capture_are_skipped() {
        let timestamps: Vec<Hns> = (0..100).map(|i| i * 10 * HNS_PER_MS).collect();
        let frame_ms = vec![10.0; 100];
        // The capture is only 1 s long, so no +-1 s window fits.
        let saves = [(500 * HNS_PER_MS, 500 * HNS_PER_MS)];
        let a = save_spikes(&timestamps, &frame_ms, &saves, 10.0);
        assert!(a.spikes.is_empty());
        assert_eq!(a.skipped, 1);
        assert_eq!(save_spikes(&[], &[], &saves, 10.0).skipped, 1);
    }
}
