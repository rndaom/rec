//! The three commands: `run` (A/B PresentMon benchmark), `idle` (recd's idle
//! cost) and `analyze` (statistics for existing CSVs).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rec_core::ipc::{self, RecorderState, Request, Response, Status};
use rec_core::time::{Hns, qpc_now};

use crate::budget::{self, PhasePair};
use crate::capture;
use crate::cli::{AnalyzeArgs, IdleArgs, RunArgs};
use crate::presentmon;
use crate::recd::{self, Readiness, Recd};
use crate::report::{self, Params, Phase, PhaseResult, RecdStart, RunRecord, Summary};
use crate::sampler::Sampler;
use crate::stats;
use crate::sys;

/// A capture with fewer frames than this is not a benchmark: the game was not
/// presenting (wrong process name, minimised, loading screen).
const MIN_FRAMES: usize = 30;

/// Length of the clip each bench-triggered save asks for.
const SAVE_SECONDS: u32 = 10;

/// How much longer than `--duration` PresentMon may take before it is killed.
const PRESENTMON_SLACK: Duration = Duration::from_secs(30);

/// After Ctrl+C, how long PresentMon gets to close its ETW session itself
/// (it shares our console, so it receives the Ctrl+C too) before it is killed.
const PRESENTMON_INTERRUPT_GRACE: Duration = Duration::from_secs(5);

/// Replay length assumed when the rec config cannot be read (rec's default).
const DEFAULT_REPLAY_SECONDS: u32 = 60;

// ---------------------------------------------------------------- analyze

pub fn analyze(args: &AnalyzeArgs) -> Result<(), String> {
    let frequency = sys::qpc_frequency();
    let mut failures = 0;
    for file in &args.files {
        println!("{}", file.display());
        let result = fs::read_to_string(file)
            .map_err(|e| format!("could not read the file: {e}"))
            .and_then(|text| capture::parse(&text, frequency, args.process.as_deref()))
            .and_then(|cap| {
                stats::compute(&cap.frame_ms)
                    .map(|s| (cap, s))
                    .ok_or_else(|| "no frames".to_string())
            });
        match result {
            Ok((cap, stats)) => {
                if let Some(app) = &cap.application {
                    println!("  application:   {app}");
                }
                println!("{}", report::describe(&stats));
                for note in &cap.notes {
                    println!("  note: {note}");
                }
            }
            Err(e) => {
                eprintln!("  error: {e}");
                failures += 1;
            }
        }
        println!();
    }
    if failures == 0 {
        Ok(())
    } else {
        Err(format!("{failures} file(s) could not be analysed"))
    }
}

// ------------------------------------------------------------------- idle

pub fn idle(args: &IdleArgs) -> Result<(), String> {
    sys::install_ctrl_c_handler();
    sys::make_std_handles_non_inheritable();
    let recd_path = args.recd.clone().unwrap_or_else(recd::default_path);
    let mut recd = Recd::new(recd_path.clone());

    let not_idle = |status: &Status| match status.state {
        RecorderState::Idle => Readiness::Ready,
        RecorderState::Recording | RecorderState::Paused => Readiness::Fail(format!(
            "recd is {:?}{}: close the game first, the idle cost can only be measured with \
             nothing being recorded",
            status.state,
            status
                .target
                .as_deref()
                .map_or(String::new(), |t| format!(" ({t})"))
        )),
        RecorderState::Error => Readiness::Fail(format!(
            "recd is in an error state: {}",
            status.reason.as_deref().unwrap_or("no reason given")
        )),
    };

    // Reuse a running recd; otherwise start one and stop it again afterwards.
    let was_running = recd::status()?.is_some();
    let measured = (|| {
        if !was_running {
            recd.start()?;
        }
        let status = recd.wait_for(recd::STATE_TIMEOUT, "idle", not_idle)?;
        let pid = recd.pid()?;
        println!(
            "measuring recd (pid {pid}) for {} s after a {} s warm-up",
            args.duration, args.warmup
        );
        sys::sleep(Duration::from_secs(u64::from(args.warmup)))?;
        let sampler = Sampler::start(pid)?;
        let slept = sys::sleep(Duration::from_secs(u64::from(args.duration)));
        let usage = sampler.finish();
        slept?;
        Ok::<_, String>((usage?, status.version))
    })();

    // Restore: only a recd we started is stopped.
    sys::clear_interrupt();
    let stopped = if was_running { Ok(()) } else { recd.stop() };

    let (usage, version) = measured?;
    stopped?;
    println!("\n{}", report::render_idle(&usage, Some(&version)));
    Ok(())
}

// -------------------------------------------------------------------- run

/// Clips saved on request during an "on" phase.
#[derive(Debug, Default)]
struct SaveLog {
    /// `(requested, completed)` on the QPC clock.
    times: Vec<(Hns, Hns)>,
    paths: Vec<PathBuf>,
    errors: Vec<String>,
}

/// Asks `recd` to save a clip every `every` until stopped.
struct Saver {
    stop: Sender<()>,
    thread: JoinHandle<SaveLog>,
}

impl Saver {
    fn start(every: Duration) -> Self {
        let (stop, rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            let mut log = SaveLog::default();
            while let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(every) {
                let requested = qpc_now();
                let request = Request::Save {
                    seconds: Some(SAVE_SECONDS),
                };
                match ipc::send(&request) {
                    Ok(Response::Saved { path }) => {
                        log.times.push((requested, qpc_now()));
                        log.paths.push(path);
                    }
                    Ok(Response::Error { message }) => log.errors.push(message),
                    Ok(other) => log.errors.push(format!("unexpected answer: {other:?}")),
                    Err(e) => log.errors.push(e),
                }
            }
            log
        });
        Self { stop, thread }
    }

    fn finish(self) -> SaveLog {
        let _ = self.stop.send(());
        self.thread.join().unwrap_or_default()
    }
}

/// Deletes the clips the bench asked for so it does not fill the user's
/// Videos folder. Only files with a video extension are touched.
fn delete_clips(paths: &[PathBuf]) -> Vec<String> {
    let mut problems = Vec::new();
    for path in paths {
        let is_video = path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
            ["mp4", "mkv", "mov"]
                .iter()
                .any(|v| e.eq_ignore_ascii_case(v))
        });
        if !is_video {
            problems.push(format!("left {} alone: not a video file", path.display()));
        } else if let Err(e) = fs::remove_file(path) {
            problems.push(format!("could not delete {}: {e}", path.display()));
        }
    }
    problems
}

/// Kills PresentMon (our own child) and ends the real-time ETW session that
/// killing it leaves running. A failure to end the session is only logged: the
/// next capture replaces a stale session anyway.
fn stop_presentmon(child: &mut Child, presentmon: &Path) {
    let _ = child.kill();
    let _ = child.wait();
    if let Err(e) = presentmon::terminate_session(presentmon) {
        eprintln!("warning: could not end PresentMon's ETW session `rec-bench`: {e}");
    }
}

/// Waits up to `grace` for `child` to exit on its own.
fn exits_within(child: &mut Child, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Waits for PresentMon, killing it (it is our own child) on timeout. On
/// Ctrl+C it is first given a few seconds to exit and close its session
/// itself. Returns its exit code.
fn wait_for_presentmon(
    child: &mut Child,
    presentmon: &Path,
    limit: Duration,
) -> Result<Option<i32>, String> {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status.code()),
            Ok(None) => {}
            Err(e) => {
                stop_presentmon(child, presentmon);
                return Err(format!("could not wait for PresentMon: {e}"));
            }
        }
        if sys::interrupted() {
            if !exits_within(child, PRESENTMON_INTERRUPT_GRACE) {
                stop_presentmon(child, presentmon);
            }
            return Err("interrupted".into());
        }
        if Instant::now() >= deadline {
            stop_presentmon(child, presentmon);
            return Err(format!(
                "PresentMon did not finish within {} s and was stopped",
                limit.as_secs()
            ));
        }
        thread::sleep(Duration::from_millis(200));
    }
}

struct Bench<'a> {
    args: &'a RunArgs,
    presentmon: PathBuf,
    recd: Recd,
    out_dir: PathBuf,
    qpc_frequency: i64,
    system: report::SystemInfo,
    /// Seconds a freshly started recd records before an "on" capture starts:
    /// at least the replay length, so its buffer is full.
    on_warmup: u32,
    /// Replay length from the rec config, if it could be read.
    replay_secs: Option<u32>,
}

impl Bench<'_> {
    fn raw_dir(&self) -> PathBuf {
        self.out_dir.join("raw")
    }

    /// Restarts recd and waits until it is recording the game. A fresh recd
    /// has an empty replay buffer, so every "on" phase starts from the same
    /// state instead of some reusing a recd whose buffer is already full.
    fn restart_recording(&mut self) -> Result<Status, String> {
        self.recd.stop()?;
        self.recd.start()?;
        let status = self
            .recd
            .wait_for(recd::STATE_TIMEOUT, "recording", |s| match s.state {
                RecorderState::Recording => Readiness::Ready,
                RecorderState::Error => Readiness::Fail(format!(
                    "recd is in an error state: {}",
                    s.reason.as_deref().unwrap_or("no reason given")
                )),
                _ => Readiness::Wait,
            })
            .map_err(|e| {
                format!(
                    "{e}\nStart the game and keep it in the foreground; recd only records \
                     once it detects it (check with `rec status`)."
                )
            })?;
        self.system.encoder = status.encoder.clone();
        self.system.recd_version = Some(status.version.clone());
        self.system.capture_target = status.target.clone();
        Ok(status)
    }

    fn run_phase(&mut self, run: u32, phase: Phase) -> Result<PhaseResult, String> {
        let args = self.args;
        println!("run {run}/{}: recd {}", args.runs, phase.name());
        let warmup = match phase {
            Phase::Off => {
                self.recd.stop()?;
                args.warmup
            }
            Phase::On => {
                self.restart_recording()?;
                self.on_warmup
            }
        };
        let mut notes = Vec::new();
        if phase == Phase::On {
            println!("  recd restarted; warming up for {warmup} s so its replay buffer fills");
        } else {
            println!("  warming up for {warmup} s");
        }
        sys::sleep(Duration::from_secs(u64::from(warmup)))?;
        let recd_start = if phase == Phase::On {
            let buffered = recd::status()?
                .ok_or("recd stopped during its warm-up")?
                .buffered_seconds;
            if let Some(replay) = self.replay_secs {
                if buffered < f64::from(replay) * 0.95 {
                    notes.push(format!(
                        "the replay buffer held {buffered:.1} s of the configured {replay} s when \
                         the capture started; raise --on-warmup"
                    ));
                }
            }
            Some(RecdStart {
                warmup_secs: warmup,
                buffered_secs: buffered,
            })
        } else {
            None
        };

        let stem = format!("run{run}-{}", phase.name());
        let csv = self.raw_dir().join(format!("{stem}.csv"));
        let log = self.raw_dir().join(format!("{stem}.presentmon.log"));
        let _ = fs::remove_file(&csv);

        println!("  capturing for {} s", args.duration);
        let mut pm = presentmon::spawn(&self.presentmon, &args.process, &csv, args.duration, &log)?;

        let sampler = if phase == Phase::On {
            match self.recd.pid().and_then(Sampler::start) {
                Ok(s) => Some(s),
                Err(e) => {
                    stop_presentmon(&mut pm, &self.presentmon);
                    return Err(e);
                }
            }
        } else {
            None
        };
        let saver = (phase == Phase::On)
            .then_some(args.save_every)
            .flatten()
            .map(|n| Saver::start(Duration::from_secs(u64::from(n))));

        let limit = Duration::from_secs(u64::from(args.duration)) + PRESENTMON_SLACK;
        let exit = wait_for_presentmon(&mut pm, &self.presentmon, limit);

        // Always stop the helpers, even if PresentMon failed.
        let saves = saver.map(Saver::finish);
        let usage = sampler.map(Sampler::finish);
        let mut saves_requested = 0;
        let mut saves_failed = 0;
        let mut save_times = Vec::new();
        if let Some(saves) = saves {
            saves_failed = saves.errors.len();
            saves_requested = saves.times.len() + saves_failed;
            save_times = saves.times;
            notes.extend(saves.errors.iter().map(|e| format!("save failed: {e}")));
            notes.extend(delete_clips(&saves.paths));
        }
        let exit = exit?;
        let usage = usage.transpose()?;

        if phase == Phase::On {
            match recd::status()? {
                Some(s) if s.state == RecorderState::Recording => {}
                other => notes.push(format!(
                    "recd was no longer recording at the end of the capture ({}): the result \
                     may be invalid",
                    other.map_or("not running".to_string(), |s| format!("{:?}", s.state))
                )),
            }
        }

        let log_text = fs::read_to_string(&log).unwrap_or_default();
        if !csv.is_file() {
            return Err(presentmon::explain_failure(exit, &log_text));
        }
        let text = fs::read_to_string(&csv)
            .map_err(|e| format!("could not read {}: {e}", csv.display()))?;
        let cap = capture::parse(&text, self.qpc_frequency, Some(&args.process)).map_err(|e| {
            let mut message = format!("{}: {e}", csv.display());
            if exit != Some(0) {
                message.push('\n');
                message.push_str(&presentmon::explain_failure(exit, &log_text));
            }
            message
        })?;
        let stats = stats::compute(&cap.frame_ms).ok_or("no frames captured")?;
        if stats.frames < MIN_FRAMES {
            return Err(format!(
                "only {} frames of {} were captured (see {}): is the game running, in the \
                 foreground and presenting frames? Check the exact process name in Task Manager.",
                stats.frames,
                args.process,
                csv.display()
            ));
        }
        if exit != Some(0) {
            notes.push(format!("PresentMon exited with code {exit:?}"));
        }
        notes.extend(cap.notes.iter().cloned());

        let (spikes, skipped) = match &cap.timestamps {
            Some(ts) if !save_times.is_empty() => {
                let a = stats::save_spikes(ts, &cap.frame_ms, &save_times, stats.median_ms);
                (a.spikes, a.skipped)
            }
            _ => (Vec::new(), save_times.len()),
        };
        println!(
            "  {} frames, {:.1} fps average, {:.1} fps 1% low",
            stats.frames, stats.avg_fps, stats.low_1_fps
        );
        if let Some(u) = &usage {
            println!(
                "  recd: {:.2}% of the machine ({:.1}% of one core), {:.0} MB private (peak)",
                u.cpu_total_pct, u.cpu_one_core_pct, u.private_peak_mb
            );
        }
        Ok(PhaseResult {
            csv: format!("raw/{stem}.csv"),
            stats,
            recd: usage,
            saves_requested,
            saves_failed,
            save_spikes: spikes,
            saves_skipped: skipped,
            recd_start,
            notes,
        })
    }

    fn execute(&mut self, records: &mut Vec<RunRecord>) -> Result<(), String> {
        for run in 1..=self.args.runs {
            // Alternate which phase goes first so slow drift (thermals, boost
            // clocks, background tasks) cancels out of the mean.
            let order = if run % 2 == 1 {
                [Phase::Off, Phase::On]
            } else {
                [Phase::On, Phase::Off]
            };
            let mut off = None;
            let mut on = None;
            for phase in order {
                let result = self.run_phase(run, phase)?;
                match phase {
                    Phase::Off => off = Some(result),
                    Phase::On => on = Some(result),
                }
            }
            let (Some(off), Some(on)) = (off, on) else {
                unreachable!("both phases ran")
            };
            records.push(RunRecord {
                run: run as usize,
                order,
                off,
                on,
            });
        }
        Ok(())
    }
}

/// `<out>/<YYYY-MM-DD>-<label>`, with `-2`, `-3`... if that already exists so
/// an earlier result is never overwritten.
fn result_dir(out: &Path, date: &str, label: &str) -> PathBuf {
    let base = format!("{date}-{}", report::sanitize_folder_name(label));
    let first = out.join(&base);
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| out.join(format!("{base}-{n}")))
        .find(|p| !p.exists())
        .expect("an unused name exists")
}

fn build_summary(
    bench: &Bench<'_>,
    date: &str,
    label: &str,
    runs: Vec<RunRecord>,
    complete: bool,
    notes: Vec<String>,
) -> Summary {
    let pairs: Vec<PhasePair> = runs
        .iter()
        .map(|r| PhasePair {
            off: r.off.stats.clone(),
            on: r.on.stats.clone(),
            recd_cpu_total_pct: r.on.recd.as_ref().map(|u| u.cpu_total_pct),
        })
        .collect();
    Summary {
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        date: date.to_string(),
        label: label.to_string(),
        process: bench.args.process.clone(),
        params: Params {
            duration_secs: bench.args.duration,
            runs: bench.args.runs,
            warmup_secs: bench.args.warmup,
            on_warmup_secs: bench.on_warmup,
            save_every_secs: bench.args.save_every,
        },
        system: bench.system.clone(),
        rec_config: report::config_summary(),
        comparison: budget::compare(&pairs),
        runs,
        complete,
        notes,
    }
}

pub fn run(args: &RunArgs) -> Result<(), String> {
    sys::install_ctrl_c_handler();
    sys::make_std_handles_non_inheritable();

    // Check everything that can fail cheaply before touching recd.
    let presentmon = presentmon::locate(args.presentmon.as_deref())?;
    let prior = recd::status()?;
    let mut recd_path = args.recd.clone().unwrap_or_else(recd::default_path);
    let prior_image = prior.as_ref().and_then(|_| recd::running_image(&recd_path));
    if !recd_path.is_file() {
        // `--recd` not given and no recd.exe beside us: a running recd tells
        // us where the user keeps it.
        match (&args.recd, &prior_image) {
            (None, Some(image)) => recd_path = image.clone(),
            _ => {
                return Err(format!(
                    "recd was not found at {} (pass --recd <path to recd.exe>)",
                    recd_path.display()
                ));
            }
        }
    }

    let date = sys::local_date();
    let label = args.label.clone().unwrap_or_else(|| args.process.clone());
    let out_dir = result_dir(&args.out, &date, &label);
    fs::create_dir_all(out_dir.join("raw"))
        .map_err(|e| format!("could not create {}: {e}", out_dir.display()))?;

    let rec_config = report::config_summary();
    let replay_secs = rec_config.as_ref().map(|c| c.replay_seconds);
    let on_warmup = args
        .on_warmup
        .unwrap_or_else(|| replay_secs.unwrap_or(DEFAULT_REPLAY_SECONDS))
        .max(args.warmup);
    let mut bench = Bench {
        args,
        presentmon,
        recd: Recd::new(recd_path.clone()),
        out_dir,
        qpc_frequency: sys::qpc_frequency(),
        system: report::system_info(),
        on_warmup,
        replay_secs,
    };
    let mut notes = Vec::new();
    if args.on_warmup.is_none() && replay_secs.is_none() {
        notes.push(format!(
            "the rec config could not be read, so the \"on\" warm-up defaulted to \
             {DEFAULT_REPLAY_SECONDS} s"
        ));
    }
    if bench.system.elevated {
        println!(
            "rec-bench is running as administrator, so the recd it starts will be elevated too"
        );
        notes.push(
            "rec-bench ran as administrator, so recd ran elevated in the \"on\" phases; an \
             elevated recd can behave differently from a normal one"
                .into(),
        );
    }

    if prior.is_some() {
        println!(
            "recd is running; it will be stopped for the benchmark and started again afterwards"
        );
    }
    let mut records = Vec::new();
    let outcome = bench.recd.stop().and_then(|()| bench.execute(&mut records));

    // Restore the user's recd (or leave none running if there was none),
    // whatever happened above.
    sys::clear_interrupt();
    let stopped = bench.recd.stop();
    let restored = if stopped.is_ok() && prior.is_some() {
        // Start it from where it was running, else from the path we used.
        let path = prior_image.as_ref().unwrap_or(&recd_path);
        if bench.system.elevated {
            // A recd started from here would carry our admin token, and its
            // pipe would then be admin-only: `rec save` from a normal terminal
            // would stop working. Leave the decision to the user.
            let line = format!(
                "rec-bench ran as administrator, so it did not start your recd again (it would \
                 run elevated and normal `rec` commands could no longer reach it). Start recd \
                 again yourself: {}",
                path.display()
            );
            println!("\n{line}");
            notes.push(line);
            Ok(())
        } else {
            recd::restore(path)
        }
    } else {
        Ok(())
    };
    if let Err(e) = &restored {
        notes.push(format!("could not start your recd again: {e}"));
    }

    let complete = outcome.is_ok() && records.len() == args.runs as usize;
    if !records.is_empty() {
        let summary = build_summary(&bench, &date, &label, records, complete, notes);
        let markdown = report::render_markdown(&summary);
        fs::write(bench.out_dir.join("summary.md"), &markdown)
            .and_then(|()| {
                fs::write(
                    bench.out_dir.join("summary.json"),
                    report::render_json(&summary),
                )
            })
            .map_err(|e| format!("could not write the summary: {e}"))?;
        println!("\n{markdown}");
        println!("results: {}", bench.out_dir.display());
    }

    outcome?;
    stopped.map_err(|e| format!("could not stop recd afterwards: {e}"))?;
    restored.map_err(|e| format!("could not restart your recd afterwards: {e}"))?;
    Ok(())
}
