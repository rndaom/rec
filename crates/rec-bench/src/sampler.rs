//! Measures one process's CPU and memory cost over a window.
//!
//! CPU comes from the difference of two `GetProcessTimes` readings (at the
//! start and end of the window), which is exact over a long window even
//! though the OS charges time per scheduler tick. Memory is sampled once a
//! second on a helper thread, because peak matters as much as average.

use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::sys::{self, Process};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const MB: f64 = 1024.0 * 1024.0;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Usage {
    pub wall_seconds: f64,
    pub cpu_seconds: f64,
    pub logical_cores: u32,
    /// CPU as a share of the whole machine (all logical cores).
    pub cpu_total_pct: f64,
    /// CPU as a share of one core (100% = one core fully busy).
    pub cpu_one_core_pct: f64,
    /// Private bytes in MiB, as Task Manager shows memory.
    pub private_peak_mb: f64,
    pub private_avg_mb: f64,
    pub samples: usize,
}

/// `(share of the whole machine, share of one core)` in percent.
pub fn cpu_percent(cpu_seconds: f64, wall_seconds: f64, logical_cores: u32) -> (f64, f64) {
    if wall_seconds <= 0.0 {
        return (0.0, 0.0);
    }
    let one_core = cpu_seconds / wall_seconds * 100.0;
    (one_core / f64::from(logical_cores.max(1)), one_core)
}

/// Builds a [`Usage`] from the raw readings.
fn summarize(
    cpu_seconds: f64,
    wall_seconds: f64,
    logical_cores: u32,
    private_bytes: &[u64],
) -> Usage {
    let (cpu_total_pct, cpu_one_core_pct) = cpu_percent(cpu_seconds, wall_seconds, logical_cores);
    let peak = private_bytes.iter().copied().max().unwrap_or(0);
    let avg = if private_bytes.is_empty() {
        0.0
    } else {
        private_bytes.iter().map(|&b| b as f64).sum::<f64>() / private_bytes.len() as f64
    };
    Usage {
        wall_seconds,
        cpu_seconds,
        logical_cores,
        cpu_total_pct,
        cpu_one_core_pct,
        private_peak_mb: peak as f64 / MB,
        private_avg_mb: avg / MB,
        samples: private_bytes.len(),
    }
}

/// A running measurement. Call [`Sampler::finish`] to end the window.
#[derive(Debug)]
pub struct Sampler {
    stop: Sender<()>,
    thread: JoinHandle<Result<Usage, String>>,
}

impl Sampler {
    /// Opens the process, takes the starting readings and begins sampling.
    pub fn start(pid: u32) -> Result<Self, String> {
        let process = Process::open(pid)?;
        let started = Instant::now();
        let cpu_start = process.cpu_seconds()?;
        let mut memory: Vec<u64> = process.private_bytes().into_iter().collect();
        let (stop, rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("rec-bench-sampler".into())
            .spawn(move || {
                // Wake once a second until told to stop (or the sender drops).
                while let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(SAMPLE_INTERVAL) {
                    // A failed read (the process exited) just skips the sample.
                    memory.extend(process.private_bytes());
                }
                let wall = started.elapsed().as_secs_f64();
                let cpu = process.cpu_seconds()? - cpu_start;
                memory.extend(process.private_bytes());
                Ok(summarize(cpu, wall, sys::logical_cores(), &memory))
            })
            .map_err(|e| format!("could not start the sampler thread: {e}"))?;
        Ok(Self { stop, thread })
    }

    /// Ends the window and returns the measurements.
    pub fn finish(self) -> Result<Usage, String> {
        // The thread may already have exited on an error; ignore a closed channel.
        let _ = self.stop.send(());
        self.thread
            .join()
            .map_err(|_| "the sampler thread panicked".to_string())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_percent_scales_by_cores() {
        // 6 s of CPU in 60 s is 10% of one core ...
        let (total, one) = cpu_percent(6.0, 60.0, 16);
        assert!((one - 10.0).abs() < 1e-9);
        // ... and 0.625% of a 16-thread machine.
        assert!((total - 0.625).abs() < 1e-9);
    }

    #[test]
    fn cpu_percent_handles_degenerate_input() {
        assert_eq!(cpu_percent(1.0, 0.0, 8), (0.0, 0.0));
        let (total, one) = cpu_percent(1.0, 1.0, 0);
        assert_eq!(total, one);
    }

    #[test]
    fn memory_peak_and_average() {
        let mb = 1024 * 1024;
        let u = summarize(0.0, 1.0, 4, &[10 * mb, 20 * mb, 30 * mb]);
        assert_eq!(u.samples, 3);
        assert!((u.private_peak_mb - 30.0).abs() < 1e-9);
        assert!((u.private_avg_mb - 20.0).abs() < 1e-9);
        assert_eq!(summarize(0.0, 1.0, 4, &[]).private_peak_mb, 0.0);
    }

    #[test]
    fn measures_this_process() {
        let sampler = Sampler::start(std::process::id()).unwrap();
        // Burn a little CPU so the counters move.
        let t = Instant::now();
        let mut x = 0u64;
        while t.elapsed() < Duration::from_millis(100) {
            x = x.wrapping_add(1);
        }
        std::hint::black_box(x);
        let usage = sampler.finish().unwrap();
        assert!(usage.wall_seconds >= 0.1);
        assert!(usage.private_peak_mb > 0.0);
        assert!(usage.samples >= 1);
    }
}
