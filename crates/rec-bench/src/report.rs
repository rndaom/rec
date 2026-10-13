//! The result of a bench run: data types, system information and the
//! `summary.md` / `summary.json` renderers.

use std::fmt::Write as _;

use rec_core::config::{self, Config};
use serde::Serialize;

use crate::budget::{self, Check, Comparison, Verdict};
use crate::sampler::Usage;
use crate::stats::{FrameStats, STUTTER_FACTOR, SaveSpike};
use crate::sys;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Off,
    On,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::Off => "off",
            Phase::On => "on",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PhaseResult {
    /// CSV path relative to the results folder.
    pub csv: String,
    pub stats: FrameStats,
    /// `recd`'s own cost during the phase ("on" phases only).
    pub recd: Option<Usage>,
    /// Saves asked of `recd`, whether or not they succeeded.
    pub saves_requested: usize,
    /// Of those, the ones that returned an error or no answer.
    pub saves_failed: usize,
    pub save_spikes: Vec<SaveSpike>,
    /// Saves whose +-1 s window was not fully inside the capture.
    pub saves_skipped: usize,
    /// How `recd` was prepared for this capture ("on" phases only).
    pub recd_start: Option<RecdStart>,
    pub notes: Vec<String>,
}

/// An "on" phase starts from a freshly started `recd` whose replay buffer has
/// had time to fill, so every "on" capture sees the same steady state.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct RecdStart {
    /// Seconds between `recd` starting to record and the capture starting.
    pub warmup_secs: u32,
    /// What `recd` reported as buffered when the capture started.
    pub buffered_secs: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunRecord {
    /// 1-based.
    pub run: usize,
    /// Which phase ran first; alternates so thermal drift cancels.
    pub order: [Phase; 2],
    pub off: PhaseResult,
    pub on: PhaseResult,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SystemInfo {
    pub cpu: Option<String>,
    pub logical_cores: u32,
    pub os: Option<String>,
    /// Whether `rec-bench` itself (and so the `recd` it started) ran as
    /// administrator.
    pub elevated: bool,
    /// `recd`'s encoder string from `rec status`; includes the GPU name.
    pub encoder: Option<String>,
    pub recd_version: Option<String>,
    /// What `recd` was capturing during the "on" phases.
    pub capture_target: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ConfigSummary {
    pub replay_seconds: u32,
    pub fps: u32,
    pub codec: String,
    pub quality: String,
    pub bitrate_kbps: Option<u32>,
    pub max_height: Option<u32>,
    pub encoder_preference: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Params {
    pub duration_secs: u32,
    pub runs: u32,
    pub warmup_secs: u32,
    /// Minimum warm-up of a freshly started `recd` before an "on" capture.
    pub on_warmup_secs: u32,
    pub save_every_secs: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Summary {
    pub tool_version: String,
    pub date: String,
    pub label: String,
    pub process: String,
    pub params: Params,
    pub system: SystemInfo,
    pub rec_config: Option<ConfigSummary>,
    pub runs: Vec<RunRecord>,
    pub comparison: Option<Comparison>,
    /// False if the bench stopped before all runs finished.
    pub complete: bool,
    pub notes: Vec<String>,
}

/// CPU, OS and logical-core count from the registry. `encoder` and the recd
/// fields are filled in later from `rec status`.
pub fn system_info() -> SystemInfo {
    const NT: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let cpu = sys::registry_string(
        r"HARDWARE\DESCRIPTION\System\CentralProcessor\0",
        "ProcessorNameString",
    )
    // The registry pads the brand string with spaces.
    .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "));
    let os = sys::registry_string(NT, "CurrentBuild").map(|build| {
        let major = sys::registry_dword(NT, "CurrentMajorVersionNumber").unwrap_or(10);
        let minor = sys::registry_dword(NT, "CurrentMinorVersionNumber").unwrap_or(0);
        let mut os = format!("Windows {major}.{minor}.{build}");
        if let Some(ubr) = sys::registry_dword(NT, "UBR") {
            let _ = write!(os, ".{ubr}");
        }
        let version = sys::registry_string(NT, "DisplayVersion")
            .or_else(|| sys::registry_string(NT, "ReleaseId"));
        if let Some(version) = version {
            let _ = write!(os, " ({version})");
        }
        os
    });
    SystemInfo {
        cpu,
        logical_cores: sys::logical_cores(),
        os,
        elevated: sys::is_elevated(),
        ..SystemInfo::default()
    }
}

/// The user's rec settings, as far as they matter for a benchmark.
pub fn config_summary() -> Option<ConfigSummary> {
    let config = Config::load(&config::default_config_path()).ok()?;
    Some(ConfigSummary {
        replay_seconds: config.replay.seconds,
        fps: config.video.fps,
        codec: format!("{:?}", config.video.codec).to_lowercase(),
        quality: format!("{:?}", config.video.quality).to_lowercase(),
        bitrate_kbps: config.video.bitrate_kbps,
        max_height: config.video.max_height,
        encoder_preference: format!("{:?}", config.video.encoder).to_lowercase(),
    })
}

/// Makes a string safe to use as a folder name.
pub fn sanitize_folder_name(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches(|c| c == '.' || c == ' ').to_string();
    if cleaned.is_empty() {
        "bench".into()
    } else {
        cleaned
    }
}

fn opt(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("unknown")
}

fn pct(value: f64) -> String {
    format!("{value:+.2}%")
}

/// A multi-line description of one capture's statistics, for `analyze`.
pub fn describe(stats: &FrameStats) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "  frames:        {} over {:.1} s",
        stats.frames, stats.duration_s
    );
    let _ = writeln!(s, "  average FPS:   {:.1}", stats.avg_fps);
    let _ = writeln!(s, "  1% low FPS:    {:.1}", stats.low_1_fps);
    let _ = writeln!(s, "  0.1% low FPS:  {:.1}", stats.low_01_fps);
    let _ = writeln!(s, "  median frame:  {:.2} ms", stats.median_ms);
    let _ = writeln!(s, "  P99 frame:     {:.2} ms", stats.p99_ms);
    let _ = writeln!(s, "  max frame:     {:.2} ms", stats.max_ms);
    let _ = write!(
        s,
        "  stutters:      {} (frames over {STUTTER_FACTOR}x the median)",
        stats.stutters
    );
    s
}

fn check_rows(out: &mut String, checks: &[Check]) {
    out.push_str("| Check | Measured | Budget | Result |\n|---|---:|---:|:---:|\n");
    for c in checks {
        let noise = c
            .uncertainty
            .map_or(String::new(), |u| format!(" \u{b1} {u:.2}{}", c.unit));
        let _ = writeln!(
            out,
            "| {} | {:.2}{}{noise} | {} {:.1}{} | {} |",
            c.name,
            c.value,
            c.unit,
            c.op,
            c.limit,
            c.unit,
            match c.verdict {
                Verdict::Pass => "pass",
                Verdict::Fail => "FAIL",
                Verdict::Inconclusive => "INCONCLUSIVE",
            }
        );
    }
}

/// Renders the idle-cost result as a Markdown table.
pub fn render_idle(usage: &Usage, recd_version: Option<&str>) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "recd idle cost over {:.0} s ({} samples){}\n",
        usage.wall_seconds,
        usage.samples,
        recd_version.map_or(String::new(), |v| format!(", recd {v}"))
    );
    out.push_str("| | |\n|---|---:|\n");
    let _ = writeln!(
        out,
        "| CPU, % of whole machine ({} logical cores) | {:.3}% |",
        usage.logical_cores, usage.cpu_total_pct
    );
    let _ = writeln!(
        out,
        "| CPU, % of one core | {:.2}% |",
        usage.cpu_one_core_pct
    );
    let _ = writeln!(
        out,
        "| Private memory, average | {:.1} MB |",
        usage.private_avg_mb
    );
    let _ = writeln!(
        out,
        "| Private memory, peak | {:.1} MB |",
        usage.private_peak_mb
    );
    out.push('\n');
    check_rows(
        &mut out,
        &budget::idle_checks(usage.cpu_total_pct, usage.private_peak_mb),
    );
    out
}

/// Renders `summary.md`.
pub fn render_markdown(s: &Summary) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "# rec-bench: {}\n", s.label);
    let _ = writeln!(o, "- Date: {}", s.date);
    let _ = writeln!(o, "- Game process: `{}`", s.process);
    let _ = write!(
        o,
        "- Capture: {} s per phase after {} s warm-up, {} run(s) of recd off and on, order \
         alternating; recd restarted before every \"on\" phase and warmed up for at least {} s",
        s.params.duration_secs, s.params.warmup_secs, s.params.runs, s.params.on_warmup_secs
    );
    if let Some(every) = s.params.save_every_secs {
        let _ = write!(o, ", a 10 s clip saved every {every} s while on");
    }
    o.push('\n');
    if !s.complete {
        let _ = writeln!(
            o,
            "- **Incomplete: the bench stopped after {} of {} run(s).**",
            s.runs.len(),
            s.params.runs
        );
    }

    o.push_str("\n## System\n\n| | |\n|---|---|\n");
    let _ = writeln!(o, "| CPU | {} |", opt(&s.system.cpu));
    let _ = writeln!(o, "| Logical cores | {} |", s.system.logical_cores);
    let _ = writeln!(o, "| OS | {} |", opt(&s.system.os));
    let _ = writeln!(
        o,
        "| Run as administrator | {} |",
        if s.system.elevated { "yes" } else { "no" }
    );
    let _ = writeln!(o, "| Encoder / GPU | {} |", opt(&s.system.encoder));
    let _ = writeln!(o, "| recd | {} |", opt(&s.system.recd_version));
    let _ = writeln!(o, "| Capture target | {} |", opt(&s.system.capture_target));

    o.push_str("\n## rec configuration\n\n");
    match &s.rec_config {
        Some(c) => {
            let _ = writeln!(
                o,
                "Replay buffer {} s, {} fps, codec {}, quality {}, bitrate {}, max height {}, \
                 encoder preference {}.",
                c.replay_seconds,
                c.fps,
                c.codec,
                c.quality,
                c.bitrate_kbps
                    .map_or("preset".to_string(), |b| format!("{b} kbit/s")),
                c.max_height
                    .map_or("native".to_string(), |h| format!("{h}p")),
                c.encoder_preference
            );
        }
        None => o.push_str("The config file could not be read.\n"),
    }

    if let Some(cmp) = &s.comparison {
        let _ = writeln!(
            o,
            "\n## Verdict against the D11 budget: {}\n",
            cmp.verdict.label()
        );
        check_rows(&mut o, &cmp.checks);
        let _ = writeln!(
            o,
            "\n\u{b1} is the noise on the mean: 2 x the standard deviation across runs / \
             sqrt(runs). A check is only decided when the mean is farther from its budget than \
             that. With fewer than {} runs the noise is unknown, so a check is only decided when \
             the mean is at least {} points (FPS loss) or {} points (CPU) from its budget.",
            budget::MIN_RUNS_FOR_NOISE,
            budget::SMALL_SAMPLE_LOSS_MARGIN_PCT,
            budget::SMALL_SAMPLE_CPU_MARGIN_PCT
        );
        if cmp.verdict == Verdict::Inconclusive {
            o.push_str(
                "\n**INCONCLUSIVE:** a check is too close to its budget to tell from these \
                 runs. Do more runs (5 or more), or use a quieter machine, and compare again.\n",
            );
        }

        o.push_str("\n## Recording on vs. off\n\n");
        o.push_str(
            "Change from off to on; negative means recording was slower.\n\n\
             | Run | Order | Avg FPS off | Avg FPS on | Change | 1% low off | 1% low on | Change \
             | 0.1% low off | 0.1% low on | Change |\n\
             |---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n",
        );
        for (run, delta) in s.runs.iter().zip(&cmp.per_run) {
            let _ = writeln!(
                o,
                "| {} | {}, {} | {:.1} | {:.1} | {} | {:.1} | {:.1} | {} | {:.1} | {:.1} | {} |",
                run.run,
                run.order[0].name(),
                run.order[1].name(),
                run.off.stats.avg_fps,
                run.on.stats.avg_fps,
                pct(delta.avg_fps_pct),
                run.off.stats.low_1_fps,
                run.on.stats.low_1_fps,
                pct(delta.low_1_pct),
                run.off.stats.low_01_fps,
                run.on.stats.low_01_fps,
                pct(delta.low_01_pct),
            );
        }
        let _ = writeln!(
            o,
            "| **Mean** | | | | **{}** | | | **{}** | | | **{}** |",
            pct(cmp.mean.avg_fps_pct),
            pct(cmp.mean.low_1_pct),
            pct(cmp.mean.low_01_pct)
        );
        let _ = writeln!(
            o,
            "| Std dev | | | | {:.2} | | | {:.2} | | | {:.2} |",
            cmp.spread.avg_fps_pct, cmp.spread.low_1_pct, cmp.spread.low_01_pct
        );
        if s.runs.len() < 3 {
            o.push_str(
                "\nFewer than 3 runs: the run-to-run noise is not known, treat small differences as noise.\n",
            );
        }
    }

    o.push_str("\n## Frame statistics\n\n");
    o.push_str(
        "| Run | recd | Frames | Avg FPS | 1% low | 0.1% low | Median ms | P99 ms | Max ms | Stutters |\n\
         |---|---|---:|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    for run in &s.runs {
        for (phase, result) in [(Phase::Off, &run.off), (Phase::On, &run.on)] {
            let st = &result.stats;
            let _ = writeln!(
                o,
                "| {} | {} | {} | {:.1} | {:.1} | {:.1} | {:.2} | {:.2} | {:.2} | {} |",
                run.run,
                phase.name(),
                st.frames,
                st.avg_fps,
                st.low_1_fps,
                st.low_01_fps,
                st.median_ms,
                st.p99_ms,
                st.max_ms,
                st.stutters
            );
        }
    }

    if s.runs.iter().any(|r| r.on.recd.is_some()) {
        o.push_str("\n## recd's own cost while recording\n\n");
        o.push_str(
            "recd was restarted before every \"on\" capture and warmed up so its replay buffer \
             was full, which makes the \"on\" captures comparable.\n\n\
             | Run | CPU, % of whole machine | CPU, % of one core | Private MB (avg) \
             | Private MB (peak) | Fresh start, warm-up (s) | Buffered at start (s) |\n\
             |---|---:|---:|---:|---:|---:|---:|\n",
        );
        for run in &s.runs {
            if let Some(u) = &run.on.recd {
                let (warmup, buffered) = run
                    .on
                    .recd_start
                    .map_or(("n/a".to_string(), "n/a".to_string()), |r| {
                        (r.warmup_secs.to_string(), format!("{:.1}", r.buffered_secs))
                    });
                let _ = writeln!(
                    o,
                    "| {} | {:.3}% | {:.2}% | {:.1} | {:.1} | {warmup} | {buffered} |",
                    run.run,
                    u.cpu_total_pct,
                    u.cpu_one_core_pct,
                    u.private_avg_mb,
                    u.private_peak_mb
                );
            }
        }
    }

    if s.runs.iter().any(|r| r.on.saves_requested > 0) {
        o.push_str("\n## Frame-time spike around clip saves\n\n");
        o.push_str("| Run | Saves requested | Saves failed |\n|---|---:|---:|\n");
        for run in &s.runs {
            let _ = writeln!(
                o,
                "| {} | {} | {} |",
                run.run, run.on.saves_requested, run.on.saves_failed
            );
        }
        o.push('\n');
        o.push_str(
            "Worst frame time within 1 s of each save (from 1 s before the request to 1 s after it \
             finished), against that capture's median frame time.\n\n\
             | Run | Save at (s) | Worst frame ms | x median |\n|---|---:|---:|---:|\n",
        );
        for run in &s.runs {
            for spike in &run.on.save_spikes {
                let _ = writeln!(
                    o,
                    "| {} | {:.1} | {:.2} | {:.1} |",
                    run.run, spike.at_s, spike.worst_ms, spike.ratio
                );
            }
            if run.on.saves_skipped > 0 {
                let _ = writeln!(
                    o,
                    "| {} | {} save(s) skipped: window outside the capture | | |",
                    run.run, run.on.saves_skipped
                );
            }
        }
        let ratios: Vec<f64> = s
            .runs
            .iter()
            .flat_map(|r| r.on.save_spikes.iter().map(|sp| sp.ratio))
            .collect();
        if let Some(worst) = ratios.iter().copied().reduce(f64::max) {
            let verdict = if worst > STUTTER_FACTOR {
                format!(
                    "at least one save coincided with a frame over {STUTTER_FACTOR}x the median; \
                     compare with the \"off\" captures' max frame time before blaming the save"
                )
            } else {
                format!("no save coincided with a frame over {STUTTER_FACTOR}x the median")
            };
            let _ = writeln!(o, "\nWorst ratio {worst:.1}x: {verdict}.");
        }
    }

    let notes: Vec<String> = s
        .notes
        .iter()
        .cloned()
        .chain(s.runs.iter().flat_map(|r| {
            [(Phase::Off, &r.off), (Phase::On, &r.on)]
                .into_iter()
                .flat_map(move |(p, res)| {
                    res.notes
                        .iter()
                        .map(move |n| format!("Run {} ({}): {n}", r.run, p.name()))
                })
        }))
        .collect();
    if !notes.is_empty() {
        o.push_str("\n## Notes\n\n");
        for note in notes {
            let _ = writeln!(o, "- {note}");
        }
    }

    o.push_str(
        "\n## Definitions\n\n\
         - Average FPS: frames divided by the sum of frame times.\n\
         - 1% / 0.1% low: 1000 divided by the mean of the slowest 1% / 0.1% of frame times.\n\
         - P99: nearest-rank 99th percentile frame time.\n\
         - Stutter: a frame longer than 2.5x the median.\n\
         - Budget (docs/decisions.md D11): at most 2% average-FPS loss, at most 2% 1%-low loss, \
         recd below 1% of the whole machine's CPU.\n\n\
         See bench/README.md for how to run a fair benchmark.\n",
    );
    o
}

pub fn render_json(s: &Summary) -> String {
    serde_json::to_string_pretty(s).expect("summary serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::{PhasePair, compare};

    fn stats(avg: f64) -> FrameStats {
        FrameStats {
            frames: 6000,
            duration_s: 60.0,
            avg_fps: avg,
            low_1_fps: avg * 0.8,
            low_01_fps: avg * 0.7,
            median_ms: 1000.0 / avg,
            p99_ms: 20.0,
            max_ms: 40.0,
            stutters: 3,
        }
    }

    fn phase(avg: f64, with_recd: bool) -> PhaseResult {
        PhaseResult {
            csv: "raw/x.csv".into(),
            stats: stats(avg),
            recd: with_recd.then_some(Usage {
                wall_seconds: 60.0,
                cpu_seconds: 3.0,
                logical_cores: 16,
                cpu_total_pct: 0.31,
                cpu_one_core_pct: 5.0,
                private_peak_mb: 120.0,
                private_avg_mb: 110.0,
                samples: 61,
            }),
            saves_requested: 3,
            saves_failed: 1,
            save_spikes: vec![SaveSpike {
                at_s: 20.0,
                worst_ms: 25.0,
                ratio: 1.5,
            }],
            saves_skipped: 1,
            recd_start: with_recd.then_some(RecdStart {
                warmup_secs: 60,
                buffered_secs: 59.5,
            }),
            notes: vec!["a note".into()],
        }
    }

    fn summary() -> Summary {
        let runs: Vec<RunRecord> = (1..=3)
            .map(|run| RunRecord {
                run,
                order: [Phase::Off, Phase::On],
                off: phase(100.0, false),
                on: phase(99.0, true),
            })
            .collect();
        let pairs: Vec<PhasePair> = runs
            .iter()
            .map(|r| PhasePair {
                off: r.off.stats.clone(),
                on: r.on.stats.clone(),
                recd_cpu_total_pct: Some(0.31),
            })
            .collect();
        Summary {
            tool_version: "0.0.1".into(),
            date: "2026-10-12".into(),
            label: "test".into(),
            process: "Game.exe".into(),
            params: Params {
                duration_secs: 60,
                runs: 3,
                warmup_secs: 5,
                on_warmup_secs: 60,
                save_every_secs: Some(20),
            },
            system: SystemInfo {
                cpu: Some("Test CPU".into()),
                logical_cores: 16,
                os: Some("Windows 10.0.26200.1".into()),
                encoder: Some("Test GPU HEVC".into()),
                ..SystemInfo::default()
            },
            rec_config: Some(ConfigSummary {
                replay_seconds: 60,
                fps: 60,
                codec: "hevc".into(),
                quality: "balanced".into(),
                bitrate_kbps: None,
                max_height: Some(1080),
                encoder_preference: "auto".into(),
            }),
            comparison: compare(&pairs),
            runs,
            complete: true,
            notes: vec![],
        }
    }

    #[test]
    fn markdown_has_every_section() {
        let md = render_markdown(&summary());
        for needle in [
            "# rec-bench: test",
            "Test CPU",
            "Test GPU HEVC",
            "Windows 10.0.26200.1",
            "bitrate preset",
            "max height 1080p",
            "Verdict against the D11 budget: PASS",
            "Saves requested",
            "Fresh start, warm-up",
            "restarted before every",
            "-1.00%",
            "recd's own cost",
            "Frame-time spike",
            "Run 1 (off): a note",
            "## Definitions",
        ] {
            assert!(md.contains(needle), "missing `{needle}` in:\n{md}");
        }
    }

    #[test]
    fn inconclusive_verdict_is_explained() {
        let mut s = summary();
        s.runs.truncate(1);
        s.comparison = compare(&[PhasePair {
            off: s.runs[0].off.stats.clone(),
            on: s.runs[0].on.stats.clone(),
            recd_cpu_total_pct: Some(0.31),
        }]);
        let md = render_markdown(&s);
        assert!(
            md.contains("Verdict against the D11 budget: INCONCLUSIVE"),
            "{md}"
        );
        assert!(md.contains("Do more runs"), "{md}");
        assert!(md.contains("Fewer than 3 runs"), "{md}");
    }

    #[test]
    fn incomplete_runs_are_flagged() {
        let mut s = summary();
        s.complete = false;
        s.params.runs = 3;
        assert!(render_markdown(&s).contains("Incomplete"));
    }

    #[test]
    fn json_round_trips_through_a_parser() {
        let json = render_json(&summary());
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["process"], "Game.exe");
        assert_eq!(value["runs"][0]["order"][0], "off");
        assert_eq!(value["comparison"]["verdict"], "pass");
    }

    #[test]
    fn folder_names_are_safe() {
        assert_eq!(sanitize_folder_name("Dead lock: 1440p"), "Dead lock_ 1440p");
        assert_eq!(sanitize_folder_name("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_folder_name("  ..  "), "bench");
        assert_eq!(sanitize_folder_name(""), "bench");
    }

    #[test]
    fn idle_report_shows_the_budget() {
        let usage = phase(0.0, true).recd.unwrap();
        let text = render_idle(&usage, Some("0.0.1"));
        assert!(text.contains("recd 0.0.1"));
        assert!(text.contains("Idle CPU"));
        assert!(
            text.contains("FAIL"),
            "0.31% and 120 MB are over the idle budget:\n{text}"
        );
    }

    #[test]
    fn describe_lists_every_metric() {
        let text = describe(&stats(100.0));
        for needle in ["average FPS", "1% low", "0.1% low", "P99", "stutters"] {
            assert!(text.contains(needle), "{text}");
        }
    }
}
