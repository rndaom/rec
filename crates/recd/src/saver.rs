//! Writes clips on a background thread at low CPU and I/O priority, so a
//! save never causes a hitch in the game.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rec_core::media::{TrackInfo, TrackKind, VIDEO_TRACK, VideoFormat};
use rec_core::replay::{Packet, ReplayBuffer};
use rec_core::time::{Hns, millis, qpc_now};
use serde_json::{Value, json};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadInformation, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
    THREAD_POWER_THROTTLING_CURRENT_VERSION, THREAD_POWER_THROTTLING_EXECUTION_SPEED,
    THREAD_POWER_THROTTLING_STATE, ThreadPowerThrottling,
};

use crate::log::warning;
use crate::sidecar::{self, ClipContext};

#[derive(Debug)]
pub struct SaveJob {
    pub buffer: Arc<Mutex<ReplayBuffer>>,
    pub tracks: Vec<TrackInfo>,
    /// The session's video segments when the save was requested, oldest
    /// first, with their formats.
    pub segments: Vec<(u32, VideoFormat)>,
    /// Clip start on the QPC clock.
    pub start: Hns,
    /// Clip end; the job waits until then (post-roll) before writing.
    pub end: Hns,
    pub dir: PathBuf,
    pub name: String,
    /// For the sidecar and the session journal.
    pub context: ClipContext,
}

/// A clip written to disk.
#[derive(Debug)]
pub struct SavedClip {
    pub path: PathBuf,
    /// The sidecar to write if the clip is starred, when `output.sidecar` is
    /// off and none was written (see `sidecar::minimal`).
    pub minimal: Option<Value>,
}

/// Runs `job` on its own thread and calls `done` with the saved clip.
pub fn spawn(job: SaveJob, done: impl FnOnce(Result<SavedClip, String>) + Send + 'static) {
    let spawned = std::thread::Builder::new()
        .name("save".into())
        .spawn(move || {
            lower_priority();
            done(run(job));
        });
    if let Err(e) = spawned {
        crate::log::error!("could not start save thread: {e}");
    }
}

/// EcoQoS plus background mode: lowest CPU, I/O and memory priority.
pub fn lower_priority() {
    let state = THREAD_POWER_THROTTLING_STATE {
        Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
    };
    // SAFETY: the state struct is valid for the size passed; the thread
    // handle is the current-thread pseudo handle.
    unsafe {
        let _ = SetThreadInformation(
            GetCurrentThread(),
            ThreadPowerThrottling,
            (&state as *const THREAD_POWER_THROTTLING_STATE).cast(),
            size_of::<THREAD_POWER_THROTTLING_STATE>() as u32,
        );
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
    }
}

fn run(job: SaveJob) -> Result<SavedClip, String> {
    // Give in-flight encoder output a moment to land in the buffer.
    let wait = job.end + millis(150) - qpc_now();
    if wait > 0 {
        std::thread::sleep(Duration::from_nanos(wait as u64 * 100));
    }
    let packets = {
        let buffer = job
            .buffer
            .lock()
            .map_err(|_| "replay buffer is unavailable")?;
        buffer.snapshot(job.start, Some(job.end))
    };
    let known: Vec<u32> = job.segments.iter().map(|&(s, _)| s).collect();
    let Some((segment, packets)) = clip_segment(packets, &known) else {
        return Err("nothing recorded yet".into());
    };
    let mut tracks = job.tracks;
    let format = job
        .segments
        .iter()
        .find(|&&(s, _)| s == segment)
        .map(|(_, format)| format.clone());
    if let Some(format) = &format {
        for track in tracks.iter_mut().filter(|t| t.id == VIDEO_TRACK) {
            track.kind = TrackKind::Video(format.clone());
        }
    }

    std::fs::create_dir_all(&job.dir)
        .map_err(|e| format!("can't create {}: {e}", job.dir.display()))?;
    let (file, path) = create_unique(&job.dir, &job.name, "mp4")
        .map_err(|e| format!("can't create a file in {}: {e}", job.dir.display()))?;
    if let Err(e) = crate::mux::write(BufWriter::new(file), tracks.clone(), &packets) {
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    let minimal =
        format.and_then(|format| describe(&path, &job.context, &tracks, &format, &packets));
    Ok(SavedClip { path, minimal })
}

/// Writes the clip's sidecar and journal line, and returns the sidecar to
/// write if the clip is starred when none was written. A failure here leaves
/// the clip itself intact, so it is only logged.
fn describe(
    path: &Path,
    ctx: &ClipContext,
    tracks: &[TrackInfo],
    format: &VideoFormat,
    packets: &[Packet],
) -> Option<Value> {
    match sidecar::describe(path, ctx, tracks, format, packets) {
        Ok(saved) => {
            ctx.session.append(json!({
                "type": "clip",
                "clip": saved.id,
                "file": path,
                "from": ctx.session.offset(saved.start),
                "to": ctx.session.offset(saved.end),
                "trigger_t": ctx.session.offset(ctx.requested),
            }));
            saved.minimal
        }
        Err(e) => {
            warning!("{}: no sidecar: {e}", path.display());
            None
        }
    }
}

/// Creates `name.ext`, or `name (2).ext`... atomically, so two quick saves
/// never write the same file.
fn create_unique(dir: &Path, name: &str, ext: &str) -> io::Result<(File, PathBuf)> {
    for n in 1.. {
        let path = if n == 1 {
            dir.join(format!("{name}.{ext}"))
        } else {
            dir.join(format!("{name} ({n}).{ext}"))
        };
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!("unbounded range")
}

/// Cuts a snapshot down to one video segment, because a clip can't change
/// resolution or codec midway (see `crate::recover`). Picks the newest of
/// `segments` (the ones known when the save was requested) with a keyframe
/// in the snapshot: a pipeline that restarted a moment ago may have none
/// yet. Audio never restarts, so it is kept for the segment's time span and
/// labelled with the segment's number. Returns `None` without video.
pub fn clip_segment(packets: Vec<Packet>, segments: &[u32]) -> Option<(u32, Vec<Packet>)> {
    let is_video = |p: &Packet| p.track == VIDEO_TRACK;
    let (segment, start) = segments.iter().rev().find_map(|&s| {
        packets
            .iter()
            .find(|p| is_video(p) && p.segment == s && p.keyframe)
            .map(|p| (s, p.pts))
    })?;
    let end = packets
        .iter()
        .filter(|p| is_video(p) && p.segment == segment)
        .map(|p| p.pts + p.duration)
        .max()?;
    let packets = packets
        .into_iter()
        .filter_map(|mut p| {
            if is_video(&p) {
                (p.segment == segment && p.pts >= start).then_some(p)
            } else {
                p.segment = segment;
                (start..end).contains(&p.pts).then_some(p)
            }
        })
        .collect();
    Some((segment, packets))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(track: u8, segment: u32, pts: Hns, keyframe: bool) -> Packet {
        Packet {
            track,
            segment,
            pts,
            dts: pts,
            duration: 10,
            keyframe,
            data: Arc::from(&[0u8][..]),
        }
    }

    /// `(track, segment, pts)` of each packet.
    fn summary(packets: &[Packet]) -> Vec<(u8, u32, Hns)> {
        packets
            .iter()
            .map(|p| (p.track, p.segment, p.pts))
            .collect()
    }

    /// Video segment 0 until 20, a restart, segment 1 from 40; audio
    /// throughout, always labelled segment 0 by the audio pipeline.
    fn restart() -> Vec<Packet> {
        vec![
            packet(0, 0, 0, true),
            packet(1, 0, 5, true),
            packet(0, 0, 10, false),
            packet(1, 0, 15, true),
            packet(1, 0, 25, true),
            packet(1, 0, 35, true),
            packet(0, 1, 40, true),
            packet(1, 0, 45, true),
            packet(0, 1, 50, false),
        ]
    }

    #[test]
    fn keeps_the_newest_segment_and_relabels_audio() {
        let (segment, kept) = clip_segment(restart(), &[0, 1]).unwrap();
        assert_eq!(segment, 1);
        assert_eq!(summary(&kept), vec![(0, 1, 40), (1, 1, 45), (0, 1, 50)]);
    }

    #[test]
    fn a_segment_without_a_keyframe_yet_falls_back_to_the_previous_one() {
        let mut packets = restart();
        packets.retain(|p| p.segment == 0);
        let (segment, kept) = clip_segment(packets, &[0, 1]).unwrap();
        assert_eq!(segment, 0);
        // Audio stops where the segment's video does.
        assert_eq!(
            summary(&kept),
            vec![(0, 0, 0), (1, 0, 5), (0, 0, 10), (1, 0, 15)]
        );
    }

    #[test]
    fn segments_started_after_the_request_are_ignored() {
        let (segment, kept) = clip_segment(restart(), &[0]).unwrap();
        assert_eq!(segment, 0);
        assert_eq!(kept.len(), 4);
    }

    #[test]
    fn nothing_without_video() {
        assert!(clip_segment(vec![packet(1, 0, 0, true)], &[0]).is_none());
        assert!(clip_segment(Vec::new(), &[0]).is_none());
    }

    #[test]
    fn create_unique_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("rec-saver-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (_, a) = create_unique(&dir, "clip", "mp4").unwrap();
        let (_, b) = create_unique(&dir, "clip", "mp4").unwrap();
        assert_ne!(a, b);
        assert!(b.ends_with("clip (2).mp4"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
