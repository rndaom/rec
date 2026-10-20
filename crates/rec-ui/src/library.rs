//! The clip library: rec's clips under the clips folder, as
//! `rec_library::Library` finds them (sidecars, and journal-only clips),
//! turned into what the UI shows. Thumbnails come from layer 2 analysis
//! (`<clip>.rec\thumb.jpg`), which a background worker runs for clips that
//! don't have it yet (see `crate::worker`).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rec_library::clip::{Clip, Source};
use rec_library::journal::Journal;
use rec_library::{Library, sys};
use serde::Serialize;
use serde_json::Value;

/// The scanned library, shared by commands and the worker.
#[derive(Debug, Default)]
pub struct Shared {
    pub inner: Mutex<State>,
}

#[derive(Debug, Default)]
pub struct State {
    pub library: Option<Library>,
}

/// Where clips are, and the session journals that describe some of them.
pub fn roots() -> (Vec<PathBuf>, PathBuf) {
    (vec![sys::clips_dir()], Journal::default_dir())
}

impl Shared {
    /// Scans (first call) or re-scans the folder and runs `f` on the result.
    pub fn with_fresh<T>(&self, f: impl FnOnce(&Library) -> T) -> T {
        let (roots, journal) = roots();
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let library = match &mut state.library {
            Some(lib) if lib.roots == roots => {
                lib.refresh(&journal);
                lib
            }
            slot => slot.insert(Library::scan(&roots, &journal)),
        };
        f(library)
    }

    /// Runs `f` on the library as last scanned, scanning if it never was.
    pub fn with<T>(&self, f: impl FnOnce(&Library) -> T) -> T {
        {
            let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(lib) = &state.library {
                return f(lib);
            }
        }
        self.with_fresh(f)
    }

    /// A copy of the clip with this key (its video path), as last scanned.
    pub fn clip(&self, key: &str) -> Result<Clip, String> {
        self.with(|lib| lib.find(key).cloned())
            .ok_or_else(|| format!("no clip {key}"))
    }

    /// The clip as it is on disk now (a re-scan re-reads only changed
    /// sidecars), for anything that shows or changes its files.
    pub fn fresh_clip(&self, key: &str) -> Result<Clip, String> {
        self.with_fresh(|lib| lib.find(key).cloned())
            .ok_or_else(|| format!("no clip {key}"))
    }
}

/// A clip in the grid.
#[derive(Debug, Serialize)]
pub struct ClipItem {
    /// The video's path; every command takes it to name the clip.
    pub key: String,
    pub id: String,
    pub game: String,
    pub file: String,
    pub folder: String,
    pub thumb: Option<PathBuf>,
    pub duration: f64,
    /// Unix seconds of the clip's start.
    pub start_utc: Option<f64>,
    /// When the file was saved, Unix seconds (for clips without a start).
    pub modified: Option<f64>,
    pub bytes: u64,
    pub starred: bool,
    pub codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<u32>,
    pub hdr: bool,
    pub markers: usize,
    pub analyzed: bool,
    pub has_sidecar: bool,
    pub trimmed_from: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Listing {
    pub folder: PathBuf,
    pub clips: Vec<ClipItem>,
    pub problems: Vec<String>,
    pub total_bytes: u64,
}

pub fn list(shared: &Shared) -> Listing {
    shared.with_fresh(|lib| {
        let mut clips: Vec<ClipItem> = lib.clips.iter().filter_map(item).collect();
        // Newest first.
        clips.sort_by(|a, b| {
            let t = |c: &ClipItem| c.start_utc.or(c.modified).unwrap_or(0.0);
            t(b).total_cmp(&t(a))
        });
        Listing {
            folder: lib.roots.first().cloned().unwrap_or_default(),
            total_bytes: clips.iter().map(|c| c.bytes).sum(),
            clips,
            problems: lib.problems.clone(),
        }
    })
}

fn item(clip: &Clip) -> Option<ClipItem> {
    let video = clip.video.as_deref()?;
    let meta = std::fs::metadata(video).ok()?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64());
    let v = clip.video_info.as_ref();
    Some(ClipItem {
        key: video.to_string_lossy().into_owned(),
        id: clip.id.clone(),
        game: clip.game.name.clone(),
        file: video
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        folder: video
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        thumb: thumb(clip),
        duration: clip.duration,
        start_utc: clip.start_utc,
        modified,
        bytes: meta.len(),
        starred: clip.starred,
        codec: v.map(|v| v.codec.clone()),
        width: v.map(|v| v.width),
        height: v.map(|v| v.height),
        fps: v.map(|v| v.fps),
        hdr: v.is_some_and(|v| v.hdr),
        markers: clip.events.iter().filter(|e| e.kind == "marker").count(),
        analyzed: clip.analysis.is_some(),
        has_sidecar: clip.source == Source::Sidecar,
        trimmed_from: clip.parent.clone(),
    })
}

/// The clip's thumbnail: analysis's, or the one the worker made for a clip
/// without a sidecar.
pub fn thumb(clip: &Clip) -> Option<PathBuf> {
    let video = clip.video.as_deref()?;
    let from_analysis = clip
        .analysis
        .as_ref()
        .and_then(|a| a["thumb"]["file"].as_str())
        .and_then(|file| video.parent().map(|d| d.join(file)));
    from_analysis
        .or_else(|| Some(cached_thumb(video)))
        .filter(|p| p.is_file())
}

/// `%LOCALAPPDATA%\rec\ui\thumbs\<hash>.jpg`, for clips without a sidecar
/// (rec doesn't write one against `output.sidecar = false`).
pub fn cached_thumb(video: &Path) -> PathBuf {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    rec_core::store::path_key(video).hash(&mut h);
    sys::data_dir()
        .join("ui")
        .join("thumbs")
        .join(format!("{:016x}.jpg", h.finish()))
}

/// Everything the player and timeline need about one clip.
#[derive(Debug, Serialize)]
pub struct Detail {
    #[serde(flatten)]
    pub item: ClipItem,
    pub video: PathBuf,
    pub sidecar: Option<PathBuf>,
    pub trigger: Option<String>,
    pub saved_at: Option<f64>,
    pub profile: Option<String>,
    pub events: Vec<Value>,
    pub tracks: Vec<TrackInfo>,
    /// Samples per second of the loudness curves.
    pub hz: u32,
    /// Notes from analysis (brightness, motion, …), if it ran.
    pub analysis: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct TrackInfo {
    pub index: usize,
    pub name: String,
    pub role: String,
    pub loudness: Vec<u8>,
}

pub fn detail(shared: &Shared, key: &str) -> Result<Detail, String> {
    let clip = shared.fresh_clip(key)?;
    let item = item(&clip).ok_or("the clip's video is gone")?;
    Ok(Detail {
        video: clip.video.clone().unwrap_or_default(),
        sidecar: clip.sidecar.clone(),
        trigger: clip.trigger.clone(),
        saved_at: clip.saved_at,
        profile: clip.game.profile.clone(),
        events: clip
            .events
            .iter()
            .map(|e| {
                let mut v = e.raw.clone();
                v["t"] = e.t.into();
                v["kind"] = e.kind.clone().into();
                v["src"] = e.src.clone().into();
                v
            })
            .collect(),
        tracks: clip
            .audio
            .iter()
            .map(|t| TrackInfo {
                index: t.index,
                name: t.name.clone(),
                role: t.role.clone(),
                loudness: t.loudness.clone(),
            })
            .collect(),
        hz: clip.hz,
        analysis: clip.analysis.clone(),
        item,
    })
}

/// Stars or unstars a clip in its sidecar (making one for a journal-only
/// clip), the same way `rec star` does.
pub fn set_starred(shared: &Shared, key: &str, starred: bool) -> Result<(), String> {
    let mut clip = shared.fresh_clip(key)?;
    rec_library::clip::set_starred(&mut clip, starred)
}

/// The files that make up a clip: the video, its sidecar and `<clip>.rec`.
pub fn files(clip: &Clip) -> Vec<PathBuf> {
    let Some(video) = clip.video.clone() else {
        return Vec::new();
    };
    let mut files = vec![video.clone()];
    files.extend(clip.sidecar.clone().filter(|p| p.exists()));
    let extras = rec_core::store::extras_dir(&video);
    if extras.is_dir() {
        files.push(extras);
    }
    files
}
