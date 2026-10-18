//! Finds clips under one or more folders: every `<clip>.rec.json` whose
//! video is still there, plus videos the session journal knows but that
//! have no sidecar. Folders made by `rec digest` and analysis (`rec-digest`,
//! `<clip>.rec`) are skipped, so bundle copies are never read as clips.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub use rec_core::store::DIGEST_DIR;
use rec_core::store::{classify, path_key as key, walk};

use crate::clip::{Clip, Event, sidecar_stem};
use crate::journal::Journal;

#[derive(Debug, Default)]
pub struct Library {
    pub roots: Vec<PathBuf>,
    /// Sorted by start time, oldest first (clips without one last).
    pub clips: Vec<Clip>,
    pub journal: Journal,
    /// Files that looked like sidecars but could not be read.
    pub problems: Vec<String>,
    /// Parsed sidecars by path and modified time, reused by `refresh`.
    cache: HashMap<PathBuf, (SystemTime, Clip)>,
}

impl Library {
    /// Clips under `roots`, with journals from `journal_dir`.
    pub fn scan(roots: &[PathBuf], journal_dir: &Path) -> Self {
        let mut library = Self {
            roots: roots.to_vec(),
            ..Self::default()
        };
        library.refresh(journal_dir);
        library
    }

    /// Scans again; unchanged sidecars are not re-parsed.
    pub fn refresh(&mut self, journal_dir: &Path) {
        self.journal = Journal::read_dir(journal_dir);
        self.problems.clear();
        let mut sidecars = Vec::new();
        let mut videos = Vec::new();
        for root in &self.roots {
            if root.is_file() {
                // A video named directly brings its sidecar.
                let sidecar = crate::clip::sidecar_for(root);
                if sidecar_stem(root).is_none() && sidecar.is_file() {
                    sidecars.push(sidecar);
                }
                classify(root, &mut sidecars, &mut videos);
            } else {
                walk(root, &mut sidecars, &mut videos);
            }
        }
        let mut cache = HashMap::new();
        let mut clips = Vec::new();
        let mut described = HashSet::new();
        for path in sidecars {
            let modified = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let clip = match self.cache.remove(&path) {
                Some((at, clip)) if at == modified => clip,
                _ => match Clip::load(&path) {
                    Ok(clip) => clip,
                    Err(e) => {
                        self.problems.push(e);
                        continue;
                    }
                },
            };
            // A sidecar without its video is a copy (a digest bundle) or
            // left behind after the clip was deleted.
            let Some(video) = clip.video.clone() else {
                continue;
            };
            described.insert(key(&video));
            cache.insert(path, (modified, clip.clone()));
            clips.push(clip);
        }
        self.cache = cache;
        for clip in &mut clips {
            fill_from_journal(clip, &self.journal);
        }
        // Videos without a sidecar that the journal knows.
        let by_file: HashMap<String, (&str, usize)> = (self.journal.sessions.iter())
            .flat_map(|(id, s)| {
                (s.clips.iter().enumerate()).map(move |(i, c)| (key(&c.file), (id.as_str(), i)))
            })
            .collect();
        for video in videos {
            if described.contains(&key(&video)) {
                continue;
            }
            let Some(&(session, i)) = by_file.get(&key(&video)) else {
                continue;
            };
            let log = &self.journal.sessions[session];
            let line = &log.clips[i];
            let markers = log
                .markers_in(line.from, line.to)
                .filter_map(|m| {
                    Event::from_value(&serde_json::json!({
                        "t": crate::clip::round3(m.t - line.from),
                        "kind": "marker",
                        "src": m.src,
                        "conf": 1,
                    }))
                })
                .collect();
            clips.push(Clip::from_journal(
                &line.id,
                &video,
                &log.id,
                line.from,
                line.to,
                line.trigger_t,
                log.game.clone(),
                log.start_utc,
                log.utc_offset_min,
                markers,
            ));
        }
        clips.sort_by(|a, b| match (a.start_utc, b.start_utc) {
            (Some(x), Some(y)) => x.total_cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.file.cmp(&b.file),
        });
        self.clips = clips;
    }

    /// A clip by id, unique id prefix (6+ characters), file name, stem or
    /// path. Ids are matched without regard to case.
    pub fn find(&self, key_text: &str) -> Option<&Clip> {
        self.position(key_text).map(|i| &self.clips[i])
    }

    pub fn position(&self, key_text: &str) -> Option<usize> {
        let wanted = key_text.trim();
        if wanted.is_empty() {
            return None;
        }
        let upper = wanted.to_ascii_uppercase();
        if let Some(i) = self.clips.iter().position(|c| c.id == upper) {
            return Some(i);
        }
        let lower = wanted.to_lowercase();
        let named = self.clips.iter().position(|c| {
            c.file.to_lowercase() == lower
                || c.stem().to_lowercase() == lower
                || (c.video.as_deref())
                    .and_then(Path::file_name)
                    .is_some_and(|n| n.to_string_lossy().to_lowercase() == lower)
                || c.video
                    .as_deref()
                    .is_some_and(|v| key(v) == key(Path::new(wanted)))
        });
        if named.is_some() {
            return named;
        }
        if wanted.len() >= 6 {
            let mut matches =
                (self.clips.iter().enumerate()).filter(|(_, c)| c.id.starts_with(&upper));
            if let (Some((i, _)), None) = (matches.next(), matches.next()) {
                return Some(i);
            }
        }
        None
    }
}

/// Session facts from the journal for a sidecar that lacks them.
fn fill_from_journal(clip: &mut Clip, journal: &Journal) {
    if clip.session_range().is_some() {
        return;
    }
    if let Some((log, line)) = journal.clip(&clip.id) {
        clip.session = Some(log.id.clone());
        clip.session_offset = Some(line.from);
        if clip.duration <= 0.0 {
            clip.duration = line.to - line.from;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::Source;
    use crate::clip::tests::sample;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rec-library-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_sidecars_and_journal_only_clips() {
        let dir = temp("scan");
        let clips = dir.join("clips");
        let game = clips.join("Team Fortress 2");
        std::fs::create_dir_all(&game).unwrap();
        // A clip with a sidecar.
        std::fs::write(game.join("a.mp4"), b"video").unwrap();
        std::fs::write(game.join("a.rec.json"), sample().to_string()).unwrap();
        // A bundle copy (no video next to it) and a digest folder.
        std::fs::create_dir_all(clips.join(DIGEST_DIR).join("a")).unwrap();
        std::fs::write(
            clips.join(DIGEST_DIR).join("a").join("clip.rec.json"),
            sample().to_string(),
        )
        .unwrap();
        std::fs::write(game.join("orphan.rec.json"), sample().to_string()).unwrap();
        // A clip only the journal knows, and a video nobody knows.
        std::fs::write(game.join("b.mp4"), b"video").unwrap();
        std::fs::write(game.join("unknown.mp4"), b"video").unwrap();
        std::fs::write(game.join("bad.rec.json"), "{").unwrap();
        let sessions = dir.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let b = game.join("b.mp4").to_string_lossy().replace('\\', "\\\\");
        std::fs::write(
            sessions.join("01S.jsonl"),
            format!(
                "{{\"type\":\"start\",\"session\":\"01S\",\"game\":\"Team Fortress 2\",\"exe\":\"tf_win64.exe\",\"utc\":\"2026-10-18T02:00:00Z\"}}\n\
                 {{\"type\":\"marker\",\"t\":15.0,\"src\":\"hotkey\"}}\n\
                 {{\"type\":\"clip\",\"clip\":\"01JOURNALONLY\",\"file\":\"{}\",\"from\":10.0,\"to\":40.0,\"trigger_t\":39.0}}\n",
                b.to_uppercase()
            ),
        )
        .unwrap();

        let library = Library::scan(std::slice::from_ref(&clips), &sessions);
        assert_eq!(library.clips.len(), 2, "{:?}", library.clips);
        assert_eq!(library.problems.len(), 1);
        let a = library.find("01TESTCLIP0000000000000000").unwrap();
        assert_eq!(a.source, Source::Sidecar);
        assert!(a.video.as_ref().unwrap().ends_with("a.mp4"));
        let b = library.find("01journalonly").unwrap();
        assert_eq!(b.source, Source::Journal);
        assert_eq!(b.duration, 30.0);
        assert_eq!(b.events.len(), 1);
        assert_eq!(b.events[0].t, 5.0);
        // Sorted by start: the sidecar clip is from 01:04, the other 02:00.
        assert_eq!(library.clips[0].id, a.id);
        // Other ways to name a clip.
        assert_eq!(library.find("a.mp4").unwrap().id, a.id);
        assert_eq!(library.find("B").unwrap().source, Source::Journal);
        assert_eq!(library.find("01TEST").unwrap().id, a.id);
        assert!(library.find("01").is_none(), "too short a prefix");
        assert!(library.find("").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn starring_is_kept_in_the_sidecar() {
        use rec_core::store::{Files, Keep};

        let dir = temp("star");
        let clips = dir.join("clips");
        std::fs::create_dir_all(&clips).unwrap();
        std::fs::write(clips.join("a.mp4"), b"video").unwrap();
        std::fs::write(clips.join("a.rec.json"), sample().to_string()).unwrap();
        std::fs::write(clips.join("b.mp4"), b"video").unwrap();
        let sessions = dir.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let b = clips.join("b.mp4").to_string_lossy().replace('\\', "\\\\");
        std::fs::write(
            sessions.join("01S.jsonl"),
            format!(
                "{{\"type\":\"start\",\"session\":\"01S\",\"game\":\"Team Fortress 2\",\"exe\":\"tf_win64.exe\",\"utc\":\"2026-10-18T02:00:00Z\"}}\n\
                 {{\"type\":\"clip\",\"clip\":\"01JOURNALONLY\",\"file\":\"{b}\",\"from\":10.0,\"to\":40.0}}\n"
            ),
        )
        .unwrap();
        let scan = || Library::scan(std::slice::from_ref(&clips), &sessions);
        let keeps = || {
            let (mut found, _) = Files::walk(&clips).clips(&sessions);
            found.sort_by(|x, y| x.video.cmp(&y.video));
            found.into_iter().map(|c| c.keep).collect::<Vec<_>>()
        };
        let mut library = scan();
        let stale = library.find("a.mp4").unwrap().clone();
        assert!(!stale.starred);
        assert_eq!(keeps(), [None, None]);

        // A clip with a sidecar: the flag is added, the rest kept.
        let i = library.position("a.mp4").unwrap();
        crate::clip::set_starred(&mut library.clips[i], true).unwrap();
        let text = std::fs::read_to_string(clips.join("a.rec.json")).unwrap();
        let raw: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(raw["starred"], true);
        assert_eq!(raw["future_field"]["kept"], true);

        // Saving analysis from a copy loaded before the star keeps it.
        crate::analysis::save(&stale).unwrap();
        assert!(scan().find("a.mp4").unwrap().starred);

        // A journal-only clip gets a sidecar built from the journal, which
        // the library still reads with the session's game.
        let i = library.position("b.mp4").unwrap();
        crate::clip::set_starred(&mut library.clips[i], true).unwrap();
        assert!(clips.join("b.rec.json").is_file());
        let library = scan();
        let b = library.find("01JOURNALONLY").unwrap();
        assert!(b.starred);
        assert_eq!(b.source, Source::Sidecar);
        assert_eq!(b.game.name, "Team Fortress 2");
        assert!(b.raw.get("source").is_none());
        assert_eq!(keeps(), [Some(Keep::Starred), Some(Keep::Starred)]);

        // Unstarring.
        let mut library = scan();
        let i = library.position("b.mp4").unwrap();
        crate::clip::set_starred(&mut library.clips[i], false).unwrap();
        assert_eq!(keeps(), [Some(Keep::Starred), None]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unstarring_a_journal_only_clip_writes_nothing() {
        let dir = temp("unstar");
        let video = dir.join("c.mp4");
        std::fs::write(&video, b"video").unwrap();
        let mut clip = Clip::from_journal(
            "01C",
            &video,
            "01S",
            0.0,
            10.0,
            None,
            Default::default(),
            None,
            None,
            Vec::new(),
        );
        crate::clip::set_starred(&mut clip, false).unwrap();
        assert!(!crate::clip::sidecar_for(&video).exists());
        // A file in the sidecar's place that isn't one is never replaced.
        std::fs::write(crate::clip::sidecar_for(&video), "{").unwrap();
        assert!(crate::clip::set_starred(&mut clip, true).is_err());
        assert_eq!(
            std::fs::read_to_string(crate::clip::sidecar_for(&video)).unwrap(),
            "{"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_single_file_root_works() {
        let dir = temp("single");
        std::fs::write(dir.join("a.mp4"), b"video").unwrap();
        std::fs::write(dir.join("a.rec.json"), sample().to_string()).unwrap();
        let library = Library::scan(&[dir.join("a.rec.json")], &dir.join("none"));
        assert_eq!(library.clips.len(), 1);
        let library = Library::scan(&[dir.join("a.mp4")], &dir.join("none"));
        assert_eq!(library.clips.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
