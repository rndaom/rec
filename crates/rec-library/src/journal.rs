//! Reads the session journals recd writes to
//! `%LOCALAPPDATA%\rec\sessions\<session>.jsonl` (one JSON object per line;
//! see "Sidecar v1 as built" in `docs/ai-clips.md`). Bad lines are skipped:
//! the journal is append-only and a crash can leave a partial last line.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::clip::Game;
use crate::time::parse_utc;

#[derive(Clone, Debug, PartialEq)]
pub struct Marker {
    /// Session seconds.
    pub t: f64,
    pub src: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClipLine {
    pub id: String,
    /// Full path of the video when it was saved.
    pub file: PathBuf,
    /// Session seconds.
    pub from: f64,
    pub to: f64,
    pub trigger_t: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionLog {
    pub id: String,
    pub game: Game,
    /// Unix seconds the session started.
    pub start_utc: Option<f64>,
    pub utc_offset_min: Option<i32>,
    pub markers: Vec<Marker>,
    pub clips: Vec<ClipLine>,
    /// Session seconds and reason, once it ended.
    pub end: Option<(f64, String)>,
}

impl SessionLog {
    /// Markers in `[from, to]` session seconds.
    pub fn markers_in(&self, from: f64, to: f64) -> impl Iterator<Item = &Marker> {
        self.markers
            .iter()
            .filter(move |m| (from..=to).contains(&m.t))
    }

    /// 1-based position of a clip among the session's clips, by start.
    pub fn clip_position(&self, id: &str) -> Option<(usize, usize)> {
        let mut starts: Vec<&ClipLine> = self.clips.iter().collect();
        starts.sort_by(|a, b| a.from.total_cmp(&b.from));
        let i = starts.iter().position(|c| c.id == id)?;
        Some((i + 1, starts.len()))
    }
}

/// Every session journal in a folder, by session id.
#[derive(Clone, Debug, Default)]
pub struct Journal {
    pub sessions: BTreeMap<String, SessionLog>,
}

impl Journal {
    /// `%LOCALAPPDATA%\rec\sessions`.
    pub fn default_dir() -> PathBuf {
        crate::sys::data_dir().join("sessions")
    }

    /// Reads every `*.jsonl` in `dir`. A missing folder is an empty journal.
    pub fn read_dir(dir: &Path) -> Self {
        let mut journal = Self::default();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return journal;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "jsonl")
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                let id = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let log = parse(&id, &text);
                journal.sessions.insert(log.id.clone(), log);
            }
        }
        journal
    }

    /// The session and line of a clip, by id.
    pub fn clip(&self, id: &str) -> Option<(&SessionLog, &ClipLine)> {
        self.sessions
            .values()
            .find_map(|s| s.clips.iter().find(|c| c.id == id).map(|c| (s, c)))
    }
}

/// One journal file. `id` (the file stem) is used if there is no `start`
/// line.
pub fn parse(id: &str, text: &str) -> SessionLog {
    let mut log = SessionLog {
        id: id.to_owned(),
        ..SessionLog::default()
    };
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v["type"].as_str() {
            Some("start") => {
                if let Some(s) = v["session"].as_str() {
                    log.id = s.to_owned();
                }
                log.game = Game {
                    name: v["game"].as_str().unwrap_or_default().to_owned(),
                    exe: v["exe"].as_str().unwrap_or_default().to_owned(),
                    steam_appid: v["steam_appid"].as_u64().map(|n| n as u32),
                    profile: v["profile"].as_str().map(str::to_owned),
                    player_names: Game::names(&v["player_names"]),
                };
                log.start_utc = v["utc"].as_str().and_then(parse_utc);
                log.utc_offset_min = v["utc_offset_min"].as_i64().map(|n| n as i32);
            }
            Some("marker") => {
                if let Some(t) = v["t"].as_f64() {
                    log.markers.push(Marker {
                        t,
                        src: v["src"].as_str().unwrap_or_default().to_owned(),
                    });
                }
            }
            Some("clip") => {
                if let (Some(id), Some(file), Some(from), Some(to)) = (
                    v["clip"].as_str(),
                    v["file"].as_str(),
                    v["from"].as_f64(),
                    v["to"].as_f64(),
                ) {
                    log.clips.push(ClipLine {
                        id: id.to_owned(),
                        file: PathBuf::from(file),
                        from,
                        to,
                        trigger_t: v["trigger_t"].as_f64(),
                    });
                }
            }
            Some("end") => {
                if let Some(t) = v["t"].as_f64() {
                    log.end = Some((t, v["why"].as_str().unwrap_or_default().to_owned()));
                }
            }
            _ => {}
        }
    }
    log
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"type":"start","session":"01S","game":"Team Fortress 2","exe":"tf_win64.exe","steam_appid":440,"profile":"tf2","player_names":["RNDAOM"],"utc_offset_min":-240,"utc":"2026-10-18T01:00:00.000Z"}
{"type":"marker","t":12.5,"src":"hotkey","utc":"2026-10-18T01:00:12.500Z"}
{"type":"clip","clip":"01B","file":"C:\\clips\\b.mp4","from":40.0,"to":70.0,"trigger_t":69.0,"utc":"x"}
{"type":"clip","clip":"01A","file":"C:\\clips\\a.mp4","from":0.0,"to":30.0,"trigger_t":29.0,"utc":"x"}
{"type":"marker","t":45.0,"src":"cli","utc":"x"}
{"type":"end","t":90.0,"why":"game closed","utc":"x"}
{"type":"clip","clip":"01C","fi"#;

    #[test]
    fn parses_a_journal() {
        let log = parse("file-stem", SAMPLE);
        assert_eq!(log.id, "01S");
        assert_eq!(log.game.steam_appid, Some(440));
        assert_eq!(log.game.profile.as_deref(), Some("tf2"));
        assert_eq!(log.game.player_names, ["RNDAOM"]);
        assert_eq!(log.start_utc, parse_utc("2026-10-18T01:00:00Z"));
        assert_eq!(log.utc_offset_min, Some(-240));
        assert_eq!(log.markers.len(), 2);
        assert_eq!(log.clips.len(), 2, "the partial last line is skipped");
        assert_eq!(log.clips[0].file, Path::new(r"C:\clips\b.mp4"));
        assert_eq!(log.end, Some((90.0, "game closed".into())));
        assert_eq!(log.clip_position("01B"), Some((2, 2)));
        assert_eq!(log.clip_position("01A"), Some((1, 2)));
        let inside: Vec<f64> = log.markers_in(40.0, 70.0).map(|m| m.t).collect();
        assert_eq!(inside, vec![45.0]);
    }

    #[test]
    fn reads_a_folder() {
        let dir = std::env::temp_dir().join(format!("rec-library-journal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("01S.jsonl"), SAMPLE).unwrap();
        std::fs::write(dir.join("notes.txt"), "not a journal").unwrap();
        let journal = Journal::read_dir(&dir);
        assert_eq!(journal.sessions.len(), 1);
        let (session, line) = journal.clip("01A").unwrap();
        assert_eq!(session.id, "01S");
        assert_eq!(line.to, 30.0);
        assert!(Journal::read_dir(&dir.join("missing")).sessions.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
