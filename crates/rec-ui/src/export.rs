//! The "Discord-ready" export: a clip (or its trim range) re-encoded to an
//! H.264 MP4 under a size limit by `rec_library::export`, on its own thread
//! with progress events. One export at a time.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rec_library::clip::Clip;
use rec_library::export::{self, ExportOptions, Plan};
use rec_library::sys;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::{recd, worker};

#[derive(Debug, Deserialize)]
pub struct Request {
    pub key: String,
    /// Decimal megabytes, as upload limits are given.
    pub limit_mb: f64,
    pub start: f64,
    pub end: Option<f64>,
}

impl Request {
    fn options(&self) -> ExportOptions {
        ExportOptions {
            limit_bytes: (self.limit_mb.max(0.1) * 1e6) as u64,
            start: self.start.max(0.0),
            end: self.end,
            ..ExportOptions::default()
        }
    }
}

#[derive(Debug, Serialize)]
pub struct PlanView {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub video_kbps: u32,
    pub audio_kbps: u32,
    pub estimated_bytes: u64,
    pub fits: bool,
    pub hdr: bool,
}

/// What an export of `request` would make, without making it.
pub fn plan(clip: &Clip, request: &Request) -> PlanView {
    let (w, h, fps, hdr) = clip
        .video_info
        .as_ref()
        .map_or((1920, 1080, 60, false), |v| {
            (v.width, v.height, v.fps, v.hdr)
        });
    let p: Plan = export::plan(clip.duration, w, h, fps, &request.options());
    PlanView {
        width: p.width,
        height: p.height,
        fps: p.fps,
        video_kbps: p.video_kbps,
        audio_kbps: p.audio_kbps,
        estimated_bytes: p.estimated_bytes,
        fits: p.fits,
        hdr,
    }
}

#[derive(Debug, Default)]
pub struct Jobs {
    running: AtomicBool,
    cancel: Arc<AtomicBool>,
}

impl Jobs {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[derive(Clone, Debug, Serialize)]
struct Done {
    path: PathBuf,
    bytes: u64,
    width: u32,
    height: u32,
    fps: u32,
    from_hdr: bool,
}

/// Starts exporting next to the clip (`<name> discord.mp4`); progress and
/// the result arrive as `export-progress`, `export-done` and
/// `export-failed` events.
pub fn start(app: AppHandle, jobs: Arc<Jobs>, clip: &Clip, request: Request) -> Result<(), String> {
    let video = clip.video.clone().ok_or("the clip's video is gone")?;
    if jobs.running.swap(true, Ordering::AcqRel) {
        return Err("an export is already running".into());
    }
    jobs.cancel.store(false, Ordering::Relaxed);
    let output = free_name(&video, "discord");
    let spawned = std::thread::Builder::new()
        .name("rec-ui export".into())
        .spawn(move || {
            // Out of a game's way when one is being recorded; otherwise
            // as fast as the GPU allows.
            if recd::recording() {
                worker::lower_this_thread();
            }
            let result = sys::init().and_then(|()| {
                let mut progress = |p: export::Progress| {
                    let _ = app.emit(
                        "export-progress",
                        serde_json::json!({ "fraction": p.fraction, "pass": p.pass }),
                    );
                };
                export::export(
                    &video,
                    &output,
                    &request.options(),
                    &mut progress,
                    &jobs.cancel,
                )
            });
            match result {
                Ok(done) => {
                    let _ = app.emit(
                        "export-done",
                        Done {
                            path: done.path,
                            bytes: done.bytes,
                            width: done.width,
                            height: done.height,
                            fps: done.fps,
                            from_hdr: done.from_hdr,
                        },
                    );
                }
                Err(e) => {
                    let _ = app.emit("export-failed", e);
                }
            }
            jobs.running.store(false, Ordering::Release);
        });
    spawned.map(drop).map_err(|e| e.to_string())
}

/// `<stem> <tag>.mp4` next to `video`, numbered if taken.
fn free_name(video: &Path, tag: &str) -> PathBuf {
    let stem = video
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "clip".into());
    let dir = video.parent().unwrap_or(Path::new("."));
    (1..)
        .map(|n| {
            let name = if n == 1 {
                format!("{stem} {tag}.mp4")
            } else {
                format!("{stem} {tag} {n}.mp4")
            };
            dir.join(name)
        })
        .find(|p| !p.exists())
        .expect("some name is free")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_never_overwrite() {
        let dir = std::env::temp_dir().join(format!("rec-ui-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("Game_1.mp4");
        assert_eq!(free_name(&video, "discord"), dir.join("Game_1 discord.mp4"));
        std::fs::write(dir.join("Game_1 discord.mp4"), b"x").unwrap();
        assert_eq!(
            free_name(&video, "discord"),
            dir.join("Game_1 discord 2.mp4")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn megabytes_are_decimal() {
        let r = Request {
            key: String::new(),
            limit_mb: 10.0,
            start: 0.0,
            end: None,
        };
        assert_eq!(r.options().limit_bytes, export::DEFAULT_LIMIT);
    }
}
