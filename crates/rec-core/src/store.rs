//! The clips folder on disk: which files are rec clips, which are starred,
//! and pruning the oldest clips to keep the folder under `output.quota_gb`.
//!
//! A rec clip is a video with a `<clip>.rec.json` sidecar, or one that a
//! session journal lists (clips saved with `output.sidecar = false`). Other
//! files in the folder are never counted and never deleted.
//!
//! A clip is kept, and left out of the quota, when its sidecar says
//! `"starred": true`, when the video is read-only (a way to keep a clip from
//! Explorer), or when its sidecar can't be read. Pruning deletes the oldest
//! of the rest, by the video's modified time (when it was saved), until they
//! fit. The newest clip and clips saved in the last few minutes are never
//! deleted. Deleting is permanent: the Recycle Bin is on the same drive, so
//! moving clips there would free no space.
//!
//! [`set_starred`] writes the star; recd's "star last clip" hotkey and
//! quick-panel row use it. `rec star` goes through rec-library, which writes
//! sidecars with the same [`write_sidecar`].

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use serde_json::Value;

/// Video file extensions a sidecar can sit next to.
pub const VIDEO_EXTENSIONS: [&str; 3] = ["mp4", "mkv", "mov"];

/// Name of the folder `rec digest` writes to by default.
pub const DIGEST_DIR: &str = "rec-digest";

/// `output.quota_gb` counts binary gigabytes, as Explorer shows sizes.
pub const BYTES_PER_GB: u64 = 1 << 30;

/// Clips saved this recently are never pruned: another save may still be
/// finishing, and a clip shouldn't vanish moments after it was saved.
pub const RECENT: Duration = Duration::from_secs(5 * 60);

/// `<clip>.rec.json` for `clip.mp4`.
pub fn sidecar_for(video: &Path) -> PathBuf {
    video.with_extension("rec.json")
}

/// The stem shared by a sidecar and its video: `a.rec.json` → `a`.
pub fn sidecar_stem(sidecar: &Path) -> Option<String> {
    let name = sidecar.file_name()?.to_str()?;
    let stem = name.strip_suffix(".rec.json")?;
    (!stem.is_empty()).then(|| stem.to_owned())
}

/// The video a sidecar describes: same stem (the user may have renamed
/// both), else `clip.file` in the same folder.
pub fn video_for(sidecar: &Path, file: &str) -> Option<PathBuf> {
    let dir = sidecar.parent()?;
    if let Some(stem) = sidecar_stem(sidecar) {
        for ext in VIDEO_EXTENSIONS {
            let path = dir.join(format!("{stem}.{ext}"));
            if path.is_file() {
                return Some(path);
            }
        }
    }
    // Only a bare file name: a sidecar never points outside its folder.
    if file.is_empty() || Path::new(file).file_name() != Some(file.as_ref()) {
        return None;
    }
    let named = dir.join(file);
    named.is_file().then_some(named)
}

/// `<clip>.rec`, the folder next to a clip where analysis keeps its images.
pub fn extras_dir(video: &Path) -> PathBuf {
    video.with_extension("rec")
}

pub fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| VIDEO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// A path for comparing: Windows paths match without regard to case or
/// slash direction.
pub fn path_key(path: &Path) -> String {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
    absolute.to_string_lossy().replace('/', "\\").to_lowercase()
}

/// Collects sidecars and videos under `dir`, recursively, in name order.
/// Folders made by `rec digest` and analysis (`rec-digest`, `<clip>.rec`)
/// and hidden ones (`.name`) are skipped, so copies are never read as clips.
pub fn walk(dir: &Path, sidecars: &mut Vec<PathBuf>, videos: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name != DIGEST_DIR && !name.ends_with(".rec") && !name.starts_with('.') {
                walk(&path, sidecars, videos);
            }
        } else if kind.is_file() {
            classify(&path, sidecars, videos);
        }
    }
}

/// Files `path` under sidecars or videos, or neither.
pub fn classify(path: &Path, sidecars: &mut Vec<PathBuf>, videos: &mut Vec<PathBuf>) {
    if sidecar_stem(path).is_some() {
        sidecars.push(path.to_owned());
    } else if is_video(path) {
        videos.push(path.to_owned());
    }
}

/// Why a clip is never pruned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Keep {
    /// The sidecar says `"starred": true`.
    Starred,
    /// The video is read-only.
    ReadOnly,
    /// A sidecar sits next to the video but can't be read, so whether the
    /// clip is starred is unknown.
    Unreadable(String),
}

/// A rec clip on disk.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredClip {
    pub video: PathBuf,
    pub sidecar: Option<PathBuf>,
    /// `<clip>.rec`, when it exists.
    pub extras: Option<PathBuf>,
    /// Video, sidecar and extras together.
    pub bytes: u64,
    /// The video's modified time: when it was saved.
    pub saved: SystemTime,
    pub keep: Option<Keep>,
}

/// The videos and sidecars under a folder, before any sidecar is read.
#[derive(Debug, Default)]
pub struct Files {
    videos: Vec<(PathBuf, fs::Metadata)>,
    sidecars: Vec<PathBuf>,
}

impl Files {
    pub fn walk(root: &Path) -> Self {
        let (mut sidecars, mut videos) = (Vec::new(), Vec::new());
        walk(root, &mut sidecars, &mut videos);
        let videos = videos
            .into_iter()
            .filter_map(|v| fs::metadata(&v).ok().map(|m| (v, m)))
            .collect();
        Self { videos, sidecars }
    }

    /// Size of every video, rec's or not: an upper bound on what the quota
    /// counts, found without reading any sidecar.
    pub fn video_bytes(&self) -> u64 {
        self.videos.iter().map(|(_, m)| m.len()).sum()
    }

    /// The rec clips among the files. Problems (unreadable sidecars) are
    /// returned as messages; those clips are kept.
    pub fn clips(self, journal_dir: &Path) -> (Vec<StoredClip>, Vec<String>) {
        let mut problems = Vec::new();
        // Video key → (sidecar, starred) for every readable sidecar.
        let mut described: HashMap<String, (PathBuf, bool)> = HashMap::new();
        // Video key → (sidecar, error) for sidecars that can't be read.
        let mut unreadable: HashMap<String, (PathBuf, String)> = HashMap::new();
        for sidecar in self.sidecars {
            match read_head(&sidecar) {
                Ok(head) => {
                    if let Some(video) = video_for(&sidecar, &head.clip.file) {
                        described.insert(path_key(&video), (sidecar, head.starred));
                    }
                }
                Err(e) => {
                    problems.push(format!("{}: {e}", sidecar.display()));
                    let Some(stem) = sidecar_stem(&sidecar) else {
                        continue;
                    };
                    let dir = sidecar.parent().unwrap_or(Path::new(""));
                    for ext in VIDEO_EXTENSIONS {
                        let video = dir.join(format!("{stem}.{ext}"));
                        unreadable.insert(path_key(&video), (sidecar.clone(), e.clone()));
                    }
                }
            }
        }
        let journaled = journal_files(journal_dir);

        let mut clips = Vec::new();
        for (video, meta) in self.videos {
            let key = path_key(&video);
            let (sidecar, mut keep) = if let Some((sidecar, starred)) = described.remove(&key) {
                (Some(sidecar), starred.then_some(Keep::Starred))
            } else if let Some((sidecar, e)) = unreadable.remove(&key) {
                (Some(sidecar), Some(Keep::Unreadable(e)))
            } else if journaled.contains(&key) {
                (None, None)
            } else {
                continue;
            };
            if keep.is_none() && meta.permissions().readonly() {
                keep = Some(Keep::ReadOnly);
            }
            let extras = Some(extras_dir(&video)).filter(|d| d.is_dir());
            let bytes = meta.len()
                + sidecar
                    .as_deref()
                    .and_then(|s| fs::metadata(s).ok())
                    .map_or(0, |m| m.len())
                + extras.as_deref().map_or(0, dir_bytes);
            clips.push(StoredClip {
                saved: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                video,
                sidecar,
                extras,
                bytes,
                keep,
            });
        }
        (clips, problems)
    }
}

/// The parts of a sidecar pruning needs. Unknown fields are skipped without
/// being kept in memory.
#[derive(Deserialize)]
struct Head {
    #[allow(dead_code)]
    rec: u64,
    clip: HeadClip,
    #[serde(default)]
    starred: bool,
}

#[derive(Deserialize)]
struct HeadClip {
    #[allow(dead_code)]
    id: String,
    #[serde(default)]
    file: String,
}

fn read_head(sidecar: &Path) -> Result<Head, String> {
    let text = fs::read_to_string(sidecar).map_err(|e| format!("can't read it: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("not a rec sidecar: {e}"))
}

/// Keys of every clip file the session journals in `dir` list.
fn journal_files(dir: &Path) -> HashSet<String> {
    let mut files = HashSet::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if v["type"] == "clip"
                && let Some(file) = v["file"].as_str()
            {
                files.insert(path_key(Path::new(file)));
            }
        }
    }
    files
}

fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_bytes(&e.path()),
            Ok(_) => e.metadata().map_or(0, |m| m.len()),
            Err(_) => 0,
        })
        .sum()
}

/// What pruning would do.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// Oldest first.
    pub delete: Vec<StoredClip>,
    /// Bytes the quota counts (clips that aren't kept), before and after.
    pub used: u64,
    pub after: u64,
    /// Bytes of kept clips, which the quota doesn't count.
    pub kept: u64,
    pub kept_clips: usize,
    /// How many of the kept clips are starred.
    pub starred: usize,
}

/// Picks the oldest clips to delete until the rest fit in `quota` bytes.
pub fn plan(clips: Vec<StoredClip>, quota: u64, now: SystemTime) -> Plan {
    let (kept, mut counted): (Vec<_>, Vec<_>) = clips.into_iter().partition(|c| c.keep.is_some());
    counted.sort_by(|a, b| a.saved.cmp(&b.saved).then_with(|| a.video.cmp(&b.video)));
    let used: u64 = counted.iter().map(|c| c.bytes).sum();
    let mut plan = Plan {
        used,
        after: used,
        kept: kept.iter().map(|c| c.bytes).sum(),
        kept_clips: kept.len(),
        starred: (kept.iter())
            .filter(|c| c.keep == Some(Keep::Starred))
            .count(),
        ..Plan::default()
    };
    // The newest clip stays even when it alone is over the quota.
    let newest = counted.len().saturating_sub(1);
    for (i, clip) in counted.into_iter().enumerate() {
        if plan.after <= quota || i == newest {
            break;
        }
        let age = now.duration_since(clip.saved).unwrap_or_default();
        if age < RECENT {
            continue;
        }
        plan.after -= clip.bytes;
        plan.delete.push(clip);
    }
    plan
}

/// Deletes a clip: the video first, then its sidecar and extras. When the
/// video can't be deleted (a player has it open), nothing is.
pub fn delete(clip: &StoredClip) -> io::Result<()> {
    match fs::remove_file(&clip.video) {
        // Already gone: another prune (`rec prune` while recd runs) got it.
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        result => result?,
    }
    if let Some(sidecar) = &clip.sidecar {
        let _ = fs::remove_file(sidecar);
    }
    if let Some(extras) = &clip.extras {
        let _ = fs::remove_dir_all(extras);
    }
    Ok(())
}

/// What a prune did.
#[derive(Debug, Default)]
pub struct Pruned {
    pub plan: Plan,
    /// Clips deleted, oldest first.
    pub deleted: Vec<StoredClip>,
    /// Clips that couldn't be deleted, with the reason.
    pub failed: Vec<(PathBuf, String)>,
    /// Sidecars that couldn't be read (their clips were kept).
    pub problems: Vec<String>,
    /// The folder was under the quota without reading any sidecar.
    pub quick: bool,
}

impl Pruned {
    pub fn freed(&self) -> u64 {
        self.deleted.iter().map(|c| c.bytes).sum()
    }
}

/// Deletes the oldest unkept clips under `root` until the rest fit in
/// `quota` bytes. `dry_run` only plans.
pub fn prune(root: &Path, journal_dir: &Path, quota: u64, dry_run: bool) -> Pruned {
    let files = Files::walk(root);
    if !dry_run && files.video_bytes() <= quota {
        return Pruned {
            quick: true,
            ..Pruned::default()
        };
    }
    let (clips, problems) = files.clips(journal_dir);
    let plan = plan(clips, quota, SystemTime::now());
    let mut pruned = Pruned {
        problems,
        ..Pruned::default()
    };
    if !dry_run {
        for clip in &plan.delete {
            match delete(clip) {
                Ok(()) => pruned.deleted.push(clip.clone()),
                Err(e) => pruned.failed.push((clip.video.clone(), e.to_string())),
            }
        }
    }
    pruned.plan = plan;
    pruned
}

/// `bytes` for people: `1.5 GB`, `320 MB`.
pub fn format_bytes(bytes: u64) -> String {
    const MB: f64 = (1 << 20) as f64;
    let gb = bytes as f64 / BYTES_PER_GB as f64;
    if gb >= 1.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{:.0} MB", bytes as f64 / MB)
    }
}

// ---- stars -------------------------------------------------------------------

/// Stars or unstars the clip at `video`, so quota pruning keeps it (or may
/// delete it again). The star is kept in the sidecar, read afresh so that
/// nothing written to it since the clip was saved (analysis) is lost. A clip
/// without a sidecar (saved with `output.sidecar = false`) gets `new`, with
/// the star: the star has to be kept somewhere. Without a `new`, starring
/// such a clip fails and unstarring it does nothing.
pub fn set_starred(video: &Path, starred: bool, new: Option<Value>) -> Result<(), String> {
    if !video.is_file() {
        return Err(format!("{} is gone", video.display()));
    }
    let path = sidecar_for(video);
    let mut raw = match fs::read_to_string(&path) {
        Ok(text) => {
            let raw: Value = serde_json::from_str(&text)
                .map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
            if !is_sidecar(&raw) {
                return Err(format!("{} is not a rec sidecar", path.display()));
            }
            raw
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => match new {
            _ if !starred => return Ok(()),
            Some(raw) if is_sidecar(&raw) => raw,
            Some(_) => return Err("the new sidecar is not a rec sidecar".into()),
            None => return Err(format!("{} has no sidecar", video.display())),
        },
        Err(e) => return Err(format!("can't read {}: {e}", path.display())),
    };
    raw["starred"] = starred.into();
    write_sidecar(&path, &raw)
}

/// What pruning needs from a sidecar to read it: `rec` and `clip.id`.
fn is_sidecar(raw: &Value) -> bool {
    raw["rec"].is_u64() && raw["clip"]["id"].is_string()
}

/// Writes a sidecar as recd does (pretty, number arrays on one line),
/// replacing the old file in one step so a reader never sees half of it.
pub fn write_sidecar(path: &Path, raw: &Value) -> Result<(), String> {
    let mut text = serde_json::to_string_pretty(raw).map_err(|e| e.to_string())?;
    text = compact_number_arrays(&text);
    text.push('\n');
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text).map_err(|e| format!("can't write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("can't replace {}: {e}", path.display())
    })
}

/// Puts arrays of plain numbers on one line, as recd writes loudness
/// curves, so a pretty-printed sidecar stays readable.
pub fn compact_number_arrays(pretty: &str) -> String {
    let mut out = String::with_capacity(pretty.len());
    let mut rest = pretty;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open..];
        let close = after.find(']');
        let body = close.map(|c| &after[1..c]);
        match body {
            Some(body)
                if !body.trim().is_empty()
                    && body.chars().all(|c| {
                        c.is_ascii_digit()
                            || c.is_whitespace()
                            || matches!(c, ',' | '.' | '-' | 'e' | 'E' | '+')
                    }) =>
            {
                let items: Vec<&str> = body.split(',').map(str::trim).collect();
                out.push('[');
                out.push_str(&items.join(","));
                out.push(']');
                rest = &after[close.unwrap_or(0) + 1..];
            }
            _ => {
                out.push('[');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Temp(PathBuf);

    impl Temp {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("rec-store-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            clear_read_only(&self.0);
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn clear_read_only(dir: &Path) {
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                clear_read_only(&path);
            } else if let Ok(meta) = fs::metadata(&path) {
                let mut perms = meta.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                let _ = fs::set_permissions(&path, perms);
            }
        }
    }

    fn write(path: &Path, bytes: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![0u8; bytes]).unwrap();
    }

    fn sidecar(video: &Path, starred: Option<bool>) {
        let file = video.file_name().unwrap().to_string_lossy();
        let mut v = serde_json::json!({ "rec": 1, "clip": { "id": "01ABC", "file": file } });
        if let Some(s) = starred {
            v["starred"] = s.into();
        }
        fs::write(sidecar_for(video), v.to_string()).unwrap();
    }

    fn stored(name: &str, bytes: u64, age_mins: u64, keep: Option<Keep>) -> StoredClip {
        StoredClip {
            video: PathBuf::from(name),
            sidecar: None,
            extras: None,
            bytes,
            saved: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 - age_mins * 60),
            keep,
        }
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000)
    }

    fn names(clips: &[StoredClip]) -> Vec<String> {
        clips
            .iter()
            .map(|c| c.video.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn plan_deletes_oldest_until_under_quota() {
        let clips = vec![
            stored("c", 10, 60, None),
            stored("a", 10, 180, None),
            stored("b", 10, 120, None),
            stored("d", 10, 30, None),
        ];
        let plan = plan(clips, 25, now());
        assert_eq!(names(&plan.delete), ["a", "b"]);
        assert_eq!((plan.used, plan.after), (40, 20));
    }

    #[test]
    fn plan_does_nothing_under_quota() {
        let plan = plan(vec![stored("a", 10, 60, None)], 10, now());
        assert!(plan.delete.is_empty());
    }

    #[test]
    fn kept_clips_are_neither_deleted_nor_counted() {
        let clips = vec![
            stored("starred", 100, 300, Some(Keep::Starred)),
            stored("readonly", 100, 300, Some(Keep::ReadOnly)),
            stored("bad", 100, 300, Some(Keep::Unreadable("x".into()))),
            stored("old", 10, 200, None),
            stored("new", 10, 100, None),
        ];
        let plan = plan(clips, 15, now());
        assert_eq!(names(&plan.delete), ["old"]);
        assert_eq!((plan.used, plan.after, plan.kept), (20, 10, 300));
        assert_eq!((plan.kept_clips, plan.starred), (3, 1));
    }

    #[test]
    fn newest_and_recent_clips_are_never_deleted() {
        // Even a zero quota leaves the newest clip, and the ones saved in
        // the last few minutes.
        let clips = vec![
            stored("old", 10, 60, None),
            stored("recent", 10, 2, None),
            stored("newest", 10, 1, None),
        ];
        let plan = plan(clips, 0, now());
        assert_eq!(names(&plan.delete), ["old"]);
        assert_eq!(plan.after, 20);
        let only = super::plan(vec![stored("only", 50, 600, None)], 0, now());
        assert!(only.delete.is_empty());
    }

    #[test]
    fn video_for_stays_in_its_folder() {
        let tmp = Temp::new("video-for");
        let video = tmp.0.join("a.mp4");
        write(&video, 1);
        assert_eq!(
            video_for(&tmp.0.join("a.rec.json"), ""),
            Some(video.clone())
        );
        // Renamed sidecar: found through `clip.file`.
        assert_eq!(video_for(&tmp.0.join("x.rec.json"), "a.mp4"), Some(video));
        let outside = tmp.0.join("sub");
        fs::create_dir_all(&outside).unwrap();
        assert_eq!(video_for(&outside.join("y.rec.json"), "..\\a.mp4"), None);
        assert_eq!(video_for(&outside.join("y.rec.json"), "../a.mp4"), None);
        let absolute = tmp.0.join("a.mp4").display().to_string();
        assert_eq!(video_for(&outside.join("y.rec.json"), &absolute), None);
    }

    #[test]
    fn finds_only_rec_clips() {
        let tmp = Temp::new("clips");
        let root = tmp.0.join("clips");
        let journals = tmp.0.join("sessions");
        fs::create_dir_all(&journals).unwrap();

        let plain = root.join("Game").join("plain.mp4");
        write(&plain, 100);
        sidecar(&plain, None);
        write(&extras_dir(&plain).join("thumb.jpg"), 7);

        let starred = root.join("Game").join("starred.mp4");
        write(&starred, 100);
        sidecar(&starred, Some(true));

        let journal_only = root.join("journal.mp4");
        write(&journal_only, 100);
        fs::write(
            journals.join("01S.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::json!({ "type": "session", "game": "x" }),
                serde_json::json!({ "type": "clip", "clip": "01C", "file": journal_only }),
            ),
        )
        .unwrap();

        let broken = root.join("broken.mp4");
        write(&broken, 100);
        fs::write(sidecar_for(&broken), "{ not json").unwrap();

        let read_only = root.join("readonly.mp4");
        write(&read_only, 100);
        sidecar(&read_only, Some(false));
        let mut perms = fs::metadata(&read_only).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&read_only, perms).unwrap();

        // Not rec's: no sidecar, not in a journal; and copies in a digest.
        write(&root.join("other.mp4"), 100);
        let copy = root.join(DIGEST_DIR).join("plain").join("plain.mp4");
        write(&copy, 100);
        sidecar(&copy, None);

        let files = Files::walk(&root);
        assert_eq!(files.video_bytes(), 600, "the digest copy is skipped");
        let (mut clips, problems) = files.clips(&journals);
        clips.sort_by(|a, b| a.video.cmp(&b.video));
        assert_eq!(problems.len(), 1, "{problems:?}");
        let summary: Vec<_> = clips
            .iter()
            .map(|c| {
                let name = c.video.file_name().unwrap().to_string_lossy().into_owned();
                (
                    name,
                    c.keep.clone().map(|k| matches!(k, Keep::Unreadable(_))),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("plain.mp4".to_string(), None),
                ("starred.mp4".to_string(), Some(false)),
                ("broken.mp4".to_string(), Some(true)),
                ("journal.mp4".to_string(), None),
                ("readonly.mp4".to_string(), Some(false)),
            ]
        );
        let starred_clip = clips.iter().find(|c| c.video == starred).unwrap();
        assert_eq!(starred_clip.keep, Some(Keep::Starred));
        let read_only_clip = clips.iter().find(|c| c.video == read_only).unwrap();
        assert_eq!(read_only_clip.keep, Some(Keep::ReadOnly));
        let plain_clip = clips.iter().find(|c| c.video == plain).unwrap();
        let sidecar_len = fs::metadata(sidecar_for(&plain)).unwrap().len();
        assert_eq!(plain_clip.bytes, 100 + sidecar_len + 7);
        assert!(plain_clip.extras.is_some());
        let journal_clip = clips.iter().find(|c| c.video == journal_only).unwrap();
        assert_eq!(
            (journal_clip.sidecar.as_ref(), journal_clip.bytes),
            (None, 100)
        );
    }

    #[test]
    fn prune_deletes_a_clip_with_its_sidecar_and_extras() {
        let tmp = Temp::new("prune");
        let root = tmp.0.join("clips");
        let old = root.join("old.mp4");
        let new = root.join("new.mp4");
        let other = root.join("other.mp4");
        for video in [&old, &new] {
            write(video, 1000);
            sidecar(video, None);
        }
        write(&extras_dir(&old).join("thumb.jpg"), 10);
        write(&other, 5000);
        let hour_ago = SystemTime::now() - Duration::from_secs(3600);
        fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(hour_ago - Duration::from_secs(60))
            .unwrap();
        fs::File::options()
            .write(true)
            .open(&new)
            .unwrap()
            .set_modified(hour_ago)
            .unwrap();
        let journals = tmp.0.join("sessions");

        // Every video fits: no sidecar is read.
        let pruned = prune(&root, &journals, 1_000_000, false);
        assert!(pruned.quick && pruned.deleted.is_empty());
        // `other.mp4` puts the folder over, but it isn't rec's, and rec's
        // clips fit.
        let pruned = prune(&root, &journals, 2500, false);
        assert!(!pruned.quick && pruned.deleted.is_empty());

        let dry = prune(&root, &journals, 1500, true);
        assert_eq!(names(&dry.plan.delete), ["old.mp4"]);
        assert!(dry.deleted.is_empty() && old.exists());

        let pruned = prune(&root, &journals, 1500, false);
        assert!(!pruned.quick);
        assert_eq!(names(&pruned.deleted), ["old.mp4"]);
        assert!(pruned.freed() > 1010);
        assert!(!old.exists() && !sidecar_for(&old).exists() && !extras_dir(&old).exists());
        assert!(new.exists() && sidecar_for(&new).exists() && other.exists());
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(format_bytes(3 * BYTES_PER_GB / 2), "1.5 GB");
        assert_eq!(format_bytes(320 << 20), "320 MB");
    }

    fn keep_of(root: &Path, journals: &Path, video: &Path) -> Option<Keep> {
        let (clips, problems) = Files::walk(root).clips(journals);
        assert!(problems.is_empty(), "{problems:?}");
        clips.into_iter().find(|c| c.video == video).unwrap().keep
    }

    #[test]
    fn starring_keeps_every_field_and_the_layout() {
        let tmp = Temp::new("star");
        let video = tmp.0.join("tf2.mp4");
        write(&video, 10);
        // As recd writes it, plus analysis added later by `rec analyze`.
        let text = "{\n  \"rec\": 1,\n  \"clip\": {\n    \"id\": \"01ABC\",\n    \"file\": \"tf2.mp4\"\n  },\n  \"game\": {\n    \"name\": \"TF2\"\n  },\n  \"audio\": [\n    {\n      \"index\": 0,\n      \"loudness\": [0,12,255]\n    }\n  ],\n  \"events\": [],\n  \"analysis\": {\n    \"cuts\": 2\n  }\n}\n";
        fs::write(sidecar_for(&video), text).unwrap();
        assert_eq!(keep_of(&tmp.0, &tmp.0, &video), None);

        set_starred(&video, true, None).unwrap();
        let starred = fs::read_to_string(sidecar_for(&video)).unwrap();
        // The same file with the star added at the end.
        let expected = text.replace("\n  }\n}\n", "\n  },\n  \"starred\": true\n}\n");
        assert_eq!(starred, expected);
        assert_eq!(keep_of(&tmp.0, &tmp.0, &video), Some(Keep::Starred));

        // Starring again changes nothing; unstarring sets it in place.
        set_starred(&video, true, None).unwrap();
        assert_eq!(fs::read_to_string(sidecar_for(&video)).unwrap(), expected);
        set_starred(&video, false, None).unwrap();
        let unstarred = fs::read_to_string(sidecar_for(&video)).unwrap();
        assert_eq!(unstarred, expected.replace("true", "false"));
        assert_eq!(keep_of(&tmp.0, &tmp.0, &video), None);
        assert!(!video.with_extension("rec.json.tmp").exists());
    }

    #[test]
    fn starring_a_clip_without_a_sidecar_writes_the_new_one() {
        let tmp = Temp::new("star-new");
        let video = tmp.0.join("a.mp4");
        write(&video, 10);
        let new = serde_json::json!({
            "rec": 1,
            "clip": { "id": "01NEW", "file": "a.mp4", "duration": 30.0 },
            "events": [],
        });

        // Unstarring writes nothing; starring with nothing to write fails.
        set_starred(&video, false, Some(new.clone())).unwrap();
        assert!(!sidecar_for(&video).exists());
        let err = set_starred(&video, true, None).unwrap_err();
        assert!(err.contains("has no sidecar"), "{err}");
        assert!(!sidecar_for(&video).exists());

        set_starred(&video, true, Some(new.clone())).unwrap();
        let written: Value =
            serde_json::from_str(&fs::read_to_string(sidecar_for(&video)).unwrap()).unwrap();
        let mut expected = new;
        expected["starred"] = true.into();
        assert_eq!(written, expected);
        assert_eq!(keep_of(&tmp.0, &tmp.0, &video), Some(Keep::Starred));
    }

    #[test]
    fn starring_refuses_what_it_cannot_keep() {
        let tmp = Temp::new("star-bad");
        let gone = tmp.0.join("gone.mp4");
        let err = set_starred(&gone, true, None).unwrap_err();
        assert!(err.contains("is gone"), "{err}");

        // A sidecar that isn't JSON, or not rec's, is left untouched.
        let video = tmp.0.join("b.mp4");
        write(&video, 10);
        for text in ["{ not json", "{\"name\": \"someone else's\"}"] {
            fs::write(sidecar_for(&video), text).unwrap();
            assert!(set_starred(&video, true, None).is_err());
            assert_eq!(fs::read_to_string(sidecar_for(&video)).unwrap(), text);
        }

        let other = tmp.0.join("c.mp4");
        write(&other, 10);
        let not_rec = serde_json::json!({ "clip": { "id": "01X" } });
        assert!(set_starred(&other, true, Some(not_rec)).is_err());
        assert!(!sidecar_for(&other).exists());
    }

    #[test]
    fn compacts_number_arrays_only() {
        let v = serde_json::json!({
            "a": [1, 2, 3], "b": [[0.5, 1.5]], "c": ["x", "y"], "d": [], "e": [{"t": 1}],
        });
        let text = compact_number_arrays(&serde_json::to_string_pretty(&v).unwrap());
        assert!(text.contains("\"a\": [1,2,3]"), "{text}");
        assert!(text.contains("[0.5,1.5]"), "{text}");
        assert!(text.contains("\"x\",\n"), "{text}");
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), v);
    }
}
