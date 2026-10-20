//! Background work for the library: layer 2 analysis (thumbnail, scene
//! cuts, loud moments) for clips that don't have it, newest first, so the
//! grid gets thumbnails and the timeline gets events.
//!
//! It must never cost a game anything: one clip at a time on a thread at
//! background priority (low CPU and I/O priority) and EcoQoS; nothing starts
//! while a game may be running (`recd::game_active`), and a clip being
//! analysed when one starts is dropped within about a second, since GPU
//! decoding isn't slowed by thread priority. It runs only while the app is
//! open, and dies with it.

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rec_library::analysis;
use rec_library::clip::Clip;
use rec_library::decode::Decoder;
use rec_library::image::Imaging;
use rec_library::sys;
use tauri::{AppHandle, Emitter};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadInformation, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
    THREAD_POWER_THROTTLING_CURRENT_VERSION, THREAD_POWER_THROTTLING_EXECUTION_SPEED,
    THREAD_POWER_THROTTLING_STATE, ThreadPowerThrottling,
};

use crate::library::{self, Shared};
use crate::recd;

/// Wakes the worker after a scan, so new clips get analysed, and lets a
/// command take a clip away from it.
#[derive(Debug, Default)]
pub struct Signal {
    pending: Mutex<bool>,
    wake: Condvar,
    /// The video being worked on.
    current: Mutex<Option<String>>,
    abort: AtomicBool,
}

impl Signal {
    /// Stops work on `key` (its file is about to be deleted or replaced)
    /// and waits, up to a few seconds, until the worker has let go of it.
    pub fn release(&self, key: &str) {
        let busy = |s: &Self| {
            let current = s.current.lock().unwrap_or_else(|e| e.into_inner());
            current
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case(key))
        };
        if !busy(self) {
            return;
        }
        self.abort.store(true, Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(5);
        while busy(self) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn working_on(&self, key: Option<String>) {
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = key;
        self.abort.store(false, Ordering::Relaxed);
    }

    pub fn poke(&self) {
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.wake.notify_one();
    }

    fn wait(&self, timeout: Duration) {
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let (mut pending, _) = self
            .wake
            .wait_timeout_while(pending, timeout, |p| !*p)
            .unwrap_or_else(|e| e.into_inner());
        *pending = false;
    }
}

/// Background priority and EcoQoS for the calling thread: efficient cores,
/// low clocks, and I/O behind everything else.
pub fn lower_this_thread() {
    let state = THREAD_POWER_THROTTLING_STATE {
        Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
    };
    // SAFETY: the current thread's pseudo-handle and a valid struct of the
    // size passed. Failure only loses the hint.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
        let _ = SetThreadInformation(
            GetCurrentThread(),
            ThreadPowerThrottling,
            (&raw const state).cast(),
            size_of::<THREAD_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

pub fn spawn(app: AppHandle, shared: Arc<Shared>, signal: Arc<Signal>) {
    let _ = std::thread::Builder::new()
        .name("rec-ui analysis".into())
        .spawn(move || run(&app, &shared, &signal));
}

fn run(app: &AppHandle, shared: &Shared, signal: &Signal) {
    lower_this_thread();
    if sys::init().is_err() {
        return;
    }
    let Ok(imaging) = Imaging::new() else {
        return;
    };
    // Clips that failed twice aren't tried again until the app restarts.
    let mut failures: Vec<String> = Vec::new();
    loop {
        signal.wait(Duration::from_secs(30));
        loop {
            if recd::game_active() {
                // Look again in a while; a scan also pokes us.
                break;
            }
            let given_up: Vec<&String> = failures
                .iter()
                .filter(|k| failures.iter().filter(|f| f == k).count() >= 2)
                .collect();
            let Some(clip) = next(shared, &given_up) else {
                break;
            };
            let key = clip
                .video
                .as_deref()
                .map(|v| v.to_string_lossy().into_owned())
                .unwrap_or_default();
            signal.working_on(Some(key.clone()));
            let game = Cell::new(false);
            let asked = Cell::new(Instant::now());
            // Every frame: a pending delete. Once a second: a game.
            let stop = || {
                if signal.abort.load(Ordering::Relaxed) {
                    return true;
                }
                if asked.get().elapsed() >= Duration::from_secs(1) {
                    asked.set(Instant::now());
                    game.set(recd::game_active());
                }
                game.get()
            };
            let result = work(&clip, &imaging, &stop);
            let stopped = signal.abort.load(Ordering::Relaxed) || game.get();
            signal.working_on(None);
            match result {
                Ok(()) => {
                    let _ = app.emit("clip-updated", &key);
                }
                // Not the clip's fault: try it again later.
                Err(_) if stopped => {}
                Err(_) => failures.push(key),
            }
        }
    }
}

/// The newest clip that has no thumbnail yet. Re-scans, so a clip just
/// analysed (its sidecar changed) is seen as done.
fn next(shared: &Shared, failed: &[&String]) -> Option<Clip> {
    shared.with_fresh(|lib| {
        lib.clips
            .iter()
            .rev()
            .filter(|c| c.video.is_some())
            .find(|c| {
                let key = c.video.as_deref().unwrap().to_string_lossy();
                library::thumb(c).is_none() && !failed.iter().any(|f| **f == key)
            })
            .cloned()
    })
}

fn work(clip: &Clip, imaging: &Imaging, stop: &dyn Fn() -> bool) -> Result<(), String> {
    if clip.sidecar.is_some() {
        let analysed = analysis::analyze_unless(clip, imaging, stop)?;
        if stop() {
            return Err(analysis::STOPPED.into());
        }
        return analysis::save(&analysed);
    }
    // No sidecar to record analysis in: just a thumbnail, kept by the app.
    let video = clip.video.as_deref().ok_or("no video")?;
    let path: PathBuf = library::cached_thumb(video);
    let mut decoder = Decoder::open(video, Some(analysis::THUMB_WIDTH))?;
    let t = (decoder.duration / 2.0).max(0.0);
    let frame = decoder.frame_at(t)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    imaging.save_jpeg(&imaging.scaled(&frame, analysis::THUMB_WIDTH)?, &path)
}
