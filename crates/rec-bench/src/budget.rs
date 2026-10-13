//! A/B comparison of "recd off" against "recd on", and the verdict against
//! the performance budget in `docs/decisions.md` (D11).
//!
//! The verdict has three values, because a mean next to a limit is not enough
//! when runs disagree with each other:
//!
//! * With 3 or more runs, the noise on a mean is `2 * s / sqrt(n)` (`s` is the
//!   sample standard deviation across runs; roughly a 95% interval). If the
//!   mean is farther from the limit than that, the verdict is PASS or FAIL.
//!   If it is within the noise, the verdict is INCONCLUSIVE: more runs, or a
//!   quieter machine, are needed to tell.
//! * With fewer than 3 runs the noise cannot be estimated, so the verdict is
//!   INCONCLUSIVE unless the mean is far from the limit
//!   ([`SMALL_SAMPLE_LOSS_MARGIN_PCT`], [`SMALL_SAMPLE_CPU_MARGIN_PCT`]):
//!   far beyond what run-to-run variation of a steady scene produces.
//! * The overall verdict is FAIL if any check fails, else INCONCLUSIVE if any
//!   is inconclusive, else PASS.

use serde::Serialize;

use crate::stats::{self, FrameStats};

/// D11: recording may cost at most this much average FPS ...
pub const MAX_AVG_FPS_LOSS_PCT: f64 = 2.0;
/// ... and this much 1%-low FPS ...
pub const MAX_LOW_1_LOSS_PCT: f64 = 2.0;
/// ... while `recd` stays below this share of the whole machine's CPU.
pub const MAX_RECORDING_CPU_PCT: f64 = 1.0;

/// D11 with no game running.
pub const MAX_IDLE_CPU_PCT: f64 = 0.2;
pub const MAX_IDLE_PRIVATE_MB: f64 = 15.0;

/// Runs needed before the spread across runs is trusted as a noise estimate.
pub const MIN_RUNS_FOR_NOISE: usize = 3;
/// The noise on a mean is this many standard errors (about a 95% interval).
const NOISE_SIGMAS: f64 = 2.0;
/// With fewer than [`MIN_RUNS_FOR_NOISE`] runs, an FPS-loss check is only
/// decided if the mean is at least this many percentage points from its
/// limit (so a loss above 7% fails, a gain of 3% or more passes). Typical
/// run-to-run noise in a steady scene is 1-2%.
pub const SMALL_SAMPLE_LOSS_MARGIN_PCT: f64 = 5.0;
/// The same for `recd`'s CPU share, in percentage points of the machine.
pub const SMALL_SAMPLE_CPU_MARGIN_PCT: f64 = 0.5;

/// One run's two phases, as the comparison needs them.
#[derive(Clone, Debug)]
pub struct PhasePair {
    pub off: FrameStats,
    pub on: FrameStats,
    /// `recd`'s CPU in the "on" phase, as % of the whole machine.
    pub recd_cpu_total_pct: Option<f64>,
}

/// Relative change from "off" to "on", in percent. Negative means "on" is
/// slower, i.e. recording cost performance.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Deltas {
    pub avg_fps_pct: f64,
    pub low_1_pct: f64,
    pub low_01_pct: f64,
}

/// The outcome of a check or of the whole comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Pass,
    Fail,
    /// The measurement is too close to the limit, relative to its noise (or
    /// there are too few runs), to say.
    Inconclusive,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
            Verdict::Inconclusive => "INCONCLUSIVE",
        }
    }

    fn from_pass(pass: bool) -> Self {
        if pass { Verdict::Pass } else { Verdict::Fail }
    }

    /// Fail beats inconclusive beats pass.
    fn overall(verdicts: impl IntoIterator<Item = Verdict>) -> Self {
        verdicts
            .into_iter()
            .fold(Verdict::Pass, |worst, v| match (worst, v) {
                (Verdict::Fail, _) | (_, Verdict::Fail) => Verdict::Fail,
                (Verdict::Inconclusive, _) | (_, Verdict::Inconclusive) => Verdict::Inconclusive,
                _ => Verdict::Pass,
            })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Check {
    pub name: &'static str,
    /// Measured value, in the unit named in `unit`.
    pub value: f64,
    pub limit: f64,
    pub unit: &'static str,
    /// How the budget reads: `<=` ("at most") or `<` ("below").
    pub op: &'static str,
    /// Noise on `value` (see the module docs); `None` when it cannot be
    /// estimated (fewer than 3 runs) or the check is a single measurement.
    pub uncertainty: Option<f64>,
    pub verdict: Verdict,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Comparison {
    pub per_run: Vec<Deltas>,
    /// Mean of the per-run deltas.
    pub mean: Deltas,
    /// Spread (sample standard deviation) of the per-run deltas, to show how
    /// much of a delta is run-to-run noise.
    pub spread: Deltas,
    pub mean_recd_cpu_total_pct: Option<f64>,
    pub checks: Vec<Check>,
    /// Worst of the checks' verdicts.
    pub verdict: Verdict,
}

/// `(on - off) / off * 100`.
pub fn delta_pct(off: f64, on: f64) -> f64 {
    (on - off) / off * 100.0
}

fn deltas(pair: &PhasePair) -> Deltas {
    Deltas {
        avg_fps_pct: delta_pct(pair.off.avg_fps, pair.on.avg_fps),
        low_1_pct: delta_pct(pair.off.low_1_fps, pair.on.low_1_fps),
        low_01_pct: delta_pct(pair.off.low_01_fps, pair.on.low_01_fps),
    }
}

fn column(all: &[Deltas], f: impl Fn(&Deltas) -> f64) -> Vec<f64> {
    all.iter().map(f).collect()
}

/// Compares every run pair. `None` if there are no runs.
pub fn compare(pairs: &[PhasePair]) -> Option<Comparison> {
    if pairs.is_empty() {
        return None;
    }
    let per_run: Vec<Deltas> = pairs.iter().map(deltas).collect();
    let avg = column(&per_run, |d| d.avg_fps_pct);
    let low1 = column(&per_run, |d| d.low_1_pct);
    let low01 = column(&per_run, |d| d.low_01_pct);
    let mean = Deltas {
        avg_fps_pct: stats::mean(&avg),
        low_1_pct: stats::mean(&low1),
        low_01_pct: stats::mean(&low01),
    };
    let spread = Deltas {
        avg_fps_pct: stats::std_dev(&avg),
        low_1_pct: stats::std_dev(&low1),
        low_01_pct: stats::std_dev(&low01),
    };
    let cpus: Vec<f64> = pairs.iter().filter_map(|p| p.recd_cpu_total_pct).collect();
    let mean_recd_cpu_total_pct = (!cpus.is_empty()).then(|| stats::mean(&cpus));

    // A gain (positive delta) is a negative loss and always passes.
    let runs = pairs.len();
    let loss_check = |name, mean_delta: f64, spread: f64, limit: f64| {
        let loss = 0.0 - mean_delta;
        let (verdict, uncertainty) = judge(
            loss,
            limit,
            true,
            spread,
            runs,
            SMALL_SAMPLE_LOSS_MARGIN_PCT,
        );
        Check {
            name,
            value: loss,
            limit,
            unit: "%",
            op: "<=",
            uncertainty,
            verdict,
        }
    };
    let mut checks = vec![
        loss_check(
            "Average FPS loss",
            mean.avg_fps_pct,
            spread.avg_fps_pct,
            MAX_AVG_FPS_LOSS_PCT,
        ),
        loss_check(
            "1% low loss",
            mean.low_1_pct,
            spread.low_1_pct,
            MAX_LOW_1_LOSS_PCT,
        ),
    ];
    if let Some(cpu) = mean_recd_cpu_total_pct {
        let (verdict, uncertainty) = judge(
            cpu,
            MAX_RECORDING_CPU_PCT,
            false,
            stats::std_dev(&cpus),
            cpus.len(),
            SMALL_SAMPLE_CPU_MARGIN_PCT,
        );
        checks.push(Check {
            name: "recd CPU (of whole machine)",
            value: cpu,
            limit: MAX_RECORDING_CPU_PCT,
            unit: "%",
            op: "<",
            uncertainty,
            verdict,
        });
    }
    let verdict = Verdict::overall(checks.iter().map(|c| c.verdict));
    Some(Comparison {
        per_run,
        mean,
        spread,
        mean_recd_cpu_total_pct,
        checks,
        verdict,
    })
}

/// Judges a mean `value` of `n` runs against `limit` (`inclusive`: the limit
/// itself passes). `spread` is the sample standard deviation across the runs.
/// Returns the verdict and the noise on the mean, if it can be estimated.
fn judge(
    value: f64,
    limit: f64,
    inclusive: bool,
    spread: f64,
    n: usize,
    small_sample_margin: f64,
) -> (Verdict, Option<f64>) {
    let meets = if inclusive {
        value <= limit
    } else {
        value < limit
    };
    let distance = (value - limit).abs();
    if n < MIN_RUNS_FOR_NOISE {
        let verdict = if distance >= small_sample_margin {
            Verdict::from_pass(meets)
        } else {
            Verdict::Inconclusive
        };
        return (verdict, None);
    }
    let noise = NOISE_SIGMAS * spread / (n as f64).sqrt();
    // Identical runs (no noise) sitting exactly on the limit are decided by
    // the limit's own rule.
    let verdict = if noise > 0.0 && distance <= noise {
        Verdict::Inconclusive
    } else {
        Verdict::from_pass(meets)
    };
    (verdict, Some(noise))
}

/// The D11 idle budget for `recd` with no game running.
pub fn idle_checks(cpu_total_pct: f64, private_mb: f64) -> Vec<Check> {
    vec![
        Check {
            name: "Idle CPU (of whole machine)",
            value: cpu_total_pct,
            limit: MAX_IDLE_CPU_PCT,
            unit: "%",
            op: "<",
            uncertainty: None,
            verdict: Verdict::from_pass(cpu_total_pct < MAX_IDLE_CPU_PCT),
        },
        Check {
            name: "Idle private memory",
            value: private_mb,
            limit: MAX_IDLE_PRIVATE_MB,
            unit: " MB",
            op: "<",
            uncertainty: None,
            verdict: Verdict::from_pass(private_mb < MAX_IDLE_PRIVATE_MB),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats_with(avg: f64, low1: f64, low01: f64) -> FrameStats {
        FrameStats {
            frames: 1000,
            duration_s: 10.0,
            avg_fps: avg,
            low_1_fps: low1,
            low_01_fps: low01,
            median_ms: 1000.0 / avg,
            p99_ms: 1000.0 / low1,
            max_ms: 1000.0 / low01,
            stutters: 0,
        }
    }

    fn pair(off: (f64, f64, f64), on: (f64, f64, f64), cpu: Option<f64>) -> PhasePair {
        PhasePair {
            off: stats_with(off.0, off.1, off.2),
            on: stats_with(on.0, on.1, on.2),
            recd_cpu_total_pct: cpu,
        }
    }

    #[test]
    fn delta_is_relative_to_off() {
        assert!((delta_pct(100.0, 99.0) + 1.0).abs() < 1e-12);
        assert!((delta_pct(200.0, 210.0) - 5.0).abs() < 1e-12);
    }

    /// One pair per entry: both average FPS and 1% low lose `loss` percent.
    fn losing(losses: &[f64], cpu: Option<f64>) -> Vec<PhasePair> {
        losses
            .iter()
            .map(|loss| {
                let f = 1.0 - loss / 100.0;
                pair((100.0, 80.0, 70.0), (100.0 * f, 80.0 * f, 70.0 * f), cpu)
            })
            .collect()
    }

    #[test]
    fn small_loss_with_little_noise_passes_the_budget() {
        let c = compare(&[
            pair((100.0, 80.0, 70.0), (99.0, 79.0, 69.0), Some(0.4)),
            pair((100.0, 80.0, 70.0), (98.5, 79.2, 69.3), Some(0.6)),
            pair((100.0, 80.0, 70.0), (99.5, 79.6, 69.5), Some(0.5)),
        ])
        .unwrap();
        assert_eq!(c.per_run.len(), 3);
        assert!((c.mean.avg_fps_pct + 1.0).abs() < 1e-9);
        assert!((c.mean_recd_cpu_total_pct.unwrap() - 0.5).abs() < 1e-12);
        assert_eq!(c.checks.len(), 3);
        assert!(c.checks.iter().all(|c| c.uncertainty.is_some()));
        assert_eq!(c.verdict, Verdict::Pass);
    }

    #[test]
    fn loss_over_two_percent_fails() {
        // Average FPS fine, 1% low down 3%, steady across runs.
        let pairs: Vec<PhasePair> = (0..3)
            .map(|i| {
                let d = f64::from(i) * 0.1;
                pair((100.0, 80.0, 70.0), (100.0, 77.6 - d, 70.0), Some(0.3))
            })
            .collect();
        let c = compare(&pairs).unwrap();
        assert_eq!(c.verdict, Verdict::Fail);
        assert_eq!(c.checks[0].verdict, Verdict::Pass);
        assert_eq!(c.checks[1].verdict, Verdict::Fail);
        assert!((c.checks[1].value - 3.125).abs() < 1e-9);
    }

    #[test]
    fn a_mean_within_the_noise_of_the_limit_is_inconclusive() {
        // Losses 1, 2 and 4%: mean 2.33%, std dev 1.53, noise 2 * 1.53 / sqrt(3)
        // = 1.76, and the mean is only 0.33 from the 2% limit.
        let c = compare(&losing(&[1.0, 2.0, 4.0], Some(0.3))).unwrap();
        assert_eq!(c.checks[0].verdict, Verdict::Inconclusive);
        assert!((c.checks[0].uncertainty.unwrap() - 1.7638).abs() < 1e-3);
        assert_eq!(c.verdict, Verdict::Inconclusive);
        // The same mean with steady runs is decided.
        let c = compare(&losing(&[2.3, 2.4, 2.3], Some(0.3))).unwrap();
        assert_eq!(c.verdict, Verdict::Fail);
    }

    #[test]
    fn a_failed_check_outranks_an_inconclusive_one() {
        // Steady 1% low loss of 6%, while the average FPS is noisy near 2%.
        let pairs = vec![
            pair((100.0, 100.0, 100.0), (99.0, 94.0, 94.0), None),
            pair((100.0, 100.0, 100.0), (98.0, 94.1, 94.0), None),
            pair((100.0, 100.0, 100.0), (96.0, 93.9, 94.0), None),
        ];
        let c = compare(&pairs).unwrap();
        assert_eq!(c.checks[0].verdict, Verdict::Inconclusive);
        assert_eq!(c.checks[1].verdict, Verdict::Fail);
        assert_eq!(c.verdict, Verdict::Fail);
    }

    #[test]
    fn exactly_at_the_limit_passes_and_a_gain_never_fails() {
        let c = compare(&vec![
            pair((100.0, 100.0, 100.0), (98.0, 98.0, 98.0), None);
            3
        ])
        .unwrap();
        assert_eq!(c.verdict, Verdict::Pass);
        let c = compare(&vec![
            pair((100.0, 80.0, 70.0), (105.0, 85.0, 75.0), None);
            3
        ])
        .unwrap();
        assert_eq!(c.verdict, Verdict::Pass);
        assert!(c.checks[0].value < 0.0);
    }

    #[test]
    fn cpu_at_or_above_one_percent_fails() {
        let c = compare(&vec![
            pair(
                (100.0, 80.0, 70.0),
                (100.0, 80.0, 70.0),
                Some(1.0)
            );
            3
        ])
        .unwrap();
        assert_eq!(c.verdict, Verdict::Fail);
        assert_eq!(c.checks[2].verdict, Verdict::Fail);
    }

    #[test]
    fn fewer_than_three_runs_are_inconclusive_unless_far_from_the_limit() {
        // 1% loss, 1.25% loss: close to the limit, noise unknown.
        let c = compare(&losing(&[1.0, 1.25], Some(0.5))).unwrap();
        assert_eq!(c.verdict, Verdict::Inconclusive);
        assert!(c.checks.iter().all(|c| c.uncertainty.is_none()));
        // A single run, 3% loss: just over the limit, still unknown.
        let c = compare(&losing(&[3.0], None)).unwrap();
        assert_eq!(c.verdict, Verdict::Inconclusive);
        // 8% loss is 6 points over the limit: decided with 1 run.
        assert_eq!(
            compare(&losing(&[8.0], None)).unwrap().verdict,
            Verdict::Fail
        );
        // A 4% gain is 6 points under it.
        assert_eq!(
            compare(&losing(&[-4.0, -4.0], None)).unwrap().verdict,
            Verdict::Pass
        );
        // CPU: 0.3% is 0.7 under the limit, 1.6% is 0.6 over it.
        assert_eq!(
            compare(&losing(&[-4.0], Some(0.3))).unwrap().verdict,
            Verdict::Pass
        );
        assert_eq!(
            compare(&losing(&[-4.0], Some(1.6))).unwrap().verdict,
            Verdict::Fail
        );
        assert_eq!(
            compare(&losing(&[-4.0], Some(0.8))).unwrap().verdict,
            Verdict::Inconclusive
        );
    }

    #[test]
    fn judge_uses_two_standard_errors() {
        // 4 runs, std dev 1: noise = 2 * 1 / 2 = 1.
        let (v, noise) = judge(1.0, 2.0, true, 1.0, 4, 5.0);
        assert_eq!((v, noise), (Verdict::Inconclusive, Some(1.0)));
        let (v, _) = judge(0.9, 2.0, true, 1.0, 4, 5.0);
        assert_eq!(v, Verdict::Pass);
        let (v, _) = judge(3.1, 2.0, true, 1.0, 4, 5.0);
        assert_eq!(v, Verdict::Fail);
    }

    #[test]
    fn missing_cpu_data_drops_that_check() {
        let c = compare(&[pair((100.0, 80.0, 70.0), (100.0, 80.0, 70.0), None)]).unwrap();
        assert_eq!(c.checks.len(), 2);
        assert!(c.mean_recd_cpu_total_pct.is_none());
    }

    #[test]
    fn no_runs_no_comparison() {
        assert!(compare(&[]).is_none());
    }

    #[test]
    fn idle_budget() {
        assert!(
            idle_checks(0.05, 9.0)
                .iter()
                .all(|c| c.verdict == Verdict::Pass)
        );
        let checks = idle_checks(0.3, 9.0);
        assert!(checks[0].verdict == Verdict::Fail && checks[1].verdict == Verdict::Pass);
        let checks = idle_checks(0.05, 15.0);
        assert!(checks[0].verdict == Verdict::Pass && checks[1].verdict == Verdict::Fail);
    }
}
