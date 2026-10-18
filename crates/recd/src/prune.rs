//! Keeps the clips folder under `output.quota_gb` (see `rec_core::store`).
//! Runs after each save, at startup and when settings change, on a
//! background thread at the same low priority as saving. Requests that
//! arrive while a prune is running are merged into one more run.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use rec_core::config::Config;
use rec_core::store::{self, BYTES_PER_GB, format_bytes};

use crate::log::{info, warning};
use crate::paths;

#[derive(Debug)]
struct Job {
    root: PathBuf,
    journal_dir: PathBuf,
    quota: u64,
}

/// `Some` while a prune thread runs: the job it should do next, if any.
static PENDING: Mutex<Option<Option<Job>>> = Mutex::new(None);

/// Unreadable sidecars already logged, so each is reported once.
static REPORTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Prunes the clips folder in the background, if a quota is set.
pub fn request(config: &Config) {
    let Some(gb) = config.output.quota_gb else {
        return;
    };
    let job = Job {
        root: paths::clips_dir(config),
        journal_dir: paths::data_dir().join("sessions"),
        quota: u64::from(gb) * BYTES_PER_GB,
    };
    let Ok(mut pending) = PENDING.lock() else {
        return;
    };
    if let Some(next) = pending.as_mut() {
        // The running thread picks it up when it finishes.
        *next = Some(job);
        return;
    }
    *pending = Some(None);
    drop(pending);
    let spawned = std::thread::Builder::new()
        .name("prune".into())
        .spawn(move || {
            crate::saver::lower_priority();
            let mut job = job;
            loop {
                run(&job);
                let Ok(mut pending) = PENDING.lock() else {
                    return;
                };
                match pending.as_mut().and_then(Option::take) {
                    Some(next) => job = next,
                    None => {
                        *pending = None;
                        return;
                    }
                }
            }
        });
    if let Err(e) = spawned {
        warning!("could not start prune thread: {e}");
        if let Ok(mut pending) = PENDING.lock() {
            *pending = None;
        }
    }
}

fn run(job: &Job) {
    let pruned = store::prune(&job.root, &job.journal_dir, job.quota, false);
    if let Ok(mut reported) = REPORTED.lock() {
        let reported = reported.get_or_insert_default();
        for problem in &pruned.problems {
            if reported.insert(problem.clone()) {
                warning!("prune: kept a clip whose sidecar is unreadable: {problem}");
            }
        }
    }
    for clip in &pruned.deleted {
        info!(
            "prune: deleted {} ({})",
            clip.video.display(),
            format_bytes(clip.bytes)
        );
    }
    for (video, e) in &pruned.failed {
        warning!("prune: couldn't delete {}: {e}", video.display());
    }
    let plan = &pruned.plan;
    if !pruned.deleted.is_empty() || !pruned.failed.is_empty() {
        info!(
            "prune: freed {}; clips now use {} of {} (plus {} in {} kept clips)",
            format_bytes(pruned.freed()),
            format_bytes(plan.used - pruned.freed()),
            format_bytes(job.quota),
            format_bytes(plan.kept),
            plan.kept_clips
        );
    } else if !pruned.quick && plan.after > job.quota {
        // Everything left is new or kept.
        warning!(
            "prune: clips use {} of {}, but none can be deleted yet",
            format_bytes(plan.after),
            format_bytes(job.quota)
        );
    }
}
