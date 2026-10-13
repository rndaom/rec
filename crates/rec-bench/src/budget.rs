//! A/B comparison of "recd off" against "recd on", and the pass/fail verdict
//! against the performance budget in `docs/decisions.md` (D11).

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

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Check {
    pub name: &'static str,
    /// Measured value, in the unit named in `unit`.
    pub value: f64,
    pub limit: f64,
    pub unit: &'static str,
    /// How the budget reads: `<=` ("at most") or `<` ("below").
    pub op: &'static str,
    pub pass: bool,
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
    /// True when every check passed.
    pub pass: bool,
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
    let mut checks = vec![
        Check {
            name: "Average FPS loss",
            value: 0.0 - mean.avg_fps_pct,
            limit: MAX_AVG_FPS_LOSS_PCT,
            unit: "%",
            op: "<=",
            pass: -mean.avg_fps_pct <= MAX_AVG_FPS_LOSS_PCT,
        },
        Check {
            name: "1% low loss",
            value: 0.0 - mean.low_1_pct,
            limit: MAX_LOW_1_LOSS_PCT,
            unit: "%",
            op: "<=",
            pass: -mean.low_1_pct <= MAX_LOW_1_LOSS_PCT,
        },
    ];
    if let Some(cpu) = mean_recd_cpu_total_pct {
        checks.push(Check {
            name: "recd CPU (of whole machine)",
            value: cpu,
            limit: MAX_RECORDING_CPU_PCT,
            unit: "%",
            op: "<",
            pass: cpu < MAX_RECORDING_CPU_PCT,
        });
    }
    let pass = checks.iter().all(|c| c.pass);
    Some(Comparison {
        per_run,
        mean,
        spread,
        mean_recd_cpu_total_pct,
        checks,
        pass,
    })
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
            pass: cpu_total_pct < MAX_IDLE_CPU_PCT,
        },
        Check {
            name: "Idle private memory",
            value: private_mb,
            limit: MAX_IDLE_PRIVATE_MB,
            unit: " MB",
            op: "<",
            pass: private_mb < MAX_IDLE_PRIVATE_MB,
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

    #[test]
    fn small_loss_passes_the_budget() {
        let c = compare(&[
            pair((100.0, 80.0, 70.0), (99.0, 79.0, 69.0), Some(0.4)),
            pair((100.0, 80.0, 70.0), (98.5, 79.2, 69.3), Some(0.6)),
        ])
        .unwrap();
        assert_eq!(c.per_run.len(), 2);
        assert!((c.mean.avg_fps_pct + 1.25).abs() < 1e-9);
        assert!((c.mean_recd_cpu_total_pct.unwrap() - 0.5).abs() < 1e-12);
        assert_eq!(c.checks.len(), 3);
        assert!(c.pass);
    }

    #[test]
    fn loss_over_two_percent_fails() {
        // Average FPS fine, 1% low down 3%.
        let c = compare(&[pair((100.0, 80.0, 70.0), (100.0, 77.6, 70.0), Some(0.3))]).unwrap();
        assert!(!c.pass);
        assert!(c.checks[0].pass);
        assert!(!c.checks[1].pass);
        assert!((c.checks[1].value - 3.0).abs() < 1e-9);
    }

    #[test]
    fn exactly_at_the_limit_passes_and_a_gain_never_fails() {
        let c = compare(&[pair((100.0, 100.0, 100.0), (98.0, 98.0, 98.0), None)]).unwrap();
        assert!(c.pass);
        let c = compare(&[pair((100.0, 80.0, 70.0), (105.0, 85.0, 75.0), None)]).unwrap();
        assert!(c.pass);
        assert!(c.checks[0].value < 0.0);
    }

    #[test]
    fn cpu_at_or_above_one_percent_fails() {
        let c = compare(&[pair((100.0, 80.0, 70.0), (100.0, 80.0, 70.0), Some(1.0))]).unwrap();
        assert!(!c.pass);
        assert!(!c.checks[2].pass);
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
        assert!(idle_checks(0.05, 9.0).iter().all(|c| c.pass));
        let checks = idle_checks(0.3, 9.0);
        assert!(!checks[0].pass && checks[1].pass);
        let checks = idle_checks(0.05, 15.0);
        assert!(checks[0].pass && !checks[1].pass);
    }
}
