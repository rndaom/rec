//! One saved clip: what its sidecar says, read tolerantly. Unknown fields
//! are kept in [`Clip::raw`] so nothing a newer recd writes is lost, and
//! missing fields stay `None` rather than failing the whole file.
//!
//! Format: "Sidecar v1 as built" in `docs/ai-clips.md`.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::time::{format_utc, parse_utc};

/// Highest sidecar version this reader understands. Newer files still load:
/// a version goes up only when a field changes meaning.
pub const SIDECAR_VERSION: u64 = 1;

/// Loudness samples per second when the file doesn't say.
pub const DEFAULT_HZ: u32 = 10;

pub use rec_core::store::{VIDEO_EXTENSIONS, sidecar_for, sidecar_stem, video_for};

/// Where a clip's facts came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Sidecar,
    /// Only the session journal knows the clip (`output.sidecar = false`, or
    /// the sidecar was deleted): no audio, loudness or video facts.
    Journal,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Game {
    pub name: String,
    pub exe: String,
    pub steam_appid: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VideoInfo {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub hdr: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    /// Index among the clip's audio streams (`0:a:<index>` in ffmpeg).
    pub index: usize,
    pub name: String,
    /// `mix`, `game`, `voice`, `other`, `mic` or `system`.
    pub role: String,
    /// One value per `1 / hz` s from t = 0: 0 is -60 dBFS or quieter, 255 is
    /// 0 dBFS, linear in dB.
    pub loudness: Vec<u8>,
}

/// An event in clip time.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub t: f64,
    pub kind: String,
    pub src: String,
    pub conf: f64,
    pub dur: Option<f64>,
    /// The event as written, with any fields beyond the ones above.
    pub raw: Value,
}

#[derive(Clone, Debug)]
pub struct Clip {
    pub source: Source,
    pub id: String,
    /// `clip.file`: the video's name when it was saved.
    pub file: String,
    /// The video on disk, if found.
    pub video: Option<PathBuf>,
    pub sidecar: Option<PathBuf>,
    pub parent: Option<String>,
    pub duration: f64,
    pub trigger: Option<String>,
    /// Clip time the save was asked for.
    pub saved_at: Option<f64>,
    pub game: Game,
    /// Unix seconds of t = 0.
    pub start_utc: Option<f64>,
    pub utc_offset_min: Option<i32>,
    pub session: Option<String>,
    /// t = 0 in session seconds.
    pub session_offset: Option<f64>,
    pub video_info: Option<VideoInfo>,
    pub audio: Vec<Track>,
    pub hz: u32,
    /// Sorted by time.
    pub events: Vec<Event>,
    /// The sidecar's `analysis` object, once layer 2 has run.
    pub analysis: Option<Value>,
    /// Kept by quota pruning (`rec star`).
    pub starred: bool,
    /// The whole file as read (or, for journal-only clips, as built).
    pub raw: Value,
}

impl Clip {
    /// Reads `<clip>.rec.json`.
    pub fn load(sidecar: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(sidecar)
            .map_err(|e| format!("can't read {}: {e}", sidecar.display()))?;
        let raw: Value = serde_json::from_str(&text)
            .map_err(|e| format!("{} is not valid JSON: {e}", sidecar.display()))?;
        let mut clip = Self::from_value(raw)
            .map_err(|e| format!("{} is not a rec sidecar: {e}", sidecar.display()))?;
        clip.sidecar = Some(sidecar.to_owned());
        clip.video = video_for(sidecar, &clip.file);
        Ok(clip)
    }

    /// Parses a sidecar already in memory. Needs `rec` and `clip.id`.
    pub fn from_value(raw: Value) -> Result<Self, String> {
        if raw.get("rec").and_then(Value::as_u64).is_none() {
            return Err("no `rec` version".into());
        }
        let clip = &raw["clip"];
        let id = str_of(&clip["id"]).ok_or("no `clip.id`")?;
        let game = &raw["game"];
        let time = &raw["time"];
        let video_info = raw.get("video").and_then(|v| {
            Some(VideoInfo {
                codec: str_of(&v["codec"]).unwrap_or_default(),
                width: v["width"].as_u64()? as u32,
                height: v["height"].as_u64()? as u32,
                fps: v["fps"].as_u64().unwrap_or(0) as u32,
                hdr: v["hdr"].as_bool().unwrap_or(false),
            })
        });
        let audio = raw["audio"]
            .as_array()
            .map(|tracks| {
                tracks
                    .iter()
                    .enumerate()
                    .map(|(i, t)| Track {
                        index: t["index"].as_u64().map_or(i, |n| n as usize),
                        name: str_of(&t["name"]).unwrap_or_default(),
                        role: str_of(&t["role"]).unwrap_or_else(|| "system".into()),
                        loudness: t["loudness"]
                            .as_array()
                            .map(|v| {
                                v.iter()
                                    .map(|x| x.as_u64().unwrap_or(0).min(255) as u8)
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut events: Vec<Event> = raw["events"]
            .as_array()
            .map(|e| e.iter().filter_map(Event::from_value).collect())
            .unwrap_or_default();
        events.sort_by(|a, b| a.t.total_cmp(&b.t));
        Ok(Self {
            source: Source::Sidecar,
            id,
            file: str_of(&clip["file"]).unwrap_or_default(),
            video: None,
            sidecar: None,
            parent: str_of(&clip["parent"]),
            duration: clip["duration"].as_f64().unwrap_or(0.0),
            trigger: str_of(&clip["trigger"]),
            saved_at: clip["saved_at"].as_f64(),
            game: Game {
                name: str_of(&game["name"]).unwrap_or_default(),
                exe: str_of(&game["exe"]).unwrap_or_default(),
                steam_appid: game["steam_appid"].as_u64().map(|n| n as u32),
            },
            start_utc: time["start_utc"].as_str().and_then(parse_utc),
            utc_offset_min: time["utc_offset_min"].as_i64().map(|n| n as i32),
            session: str_of(&time["session"]),
            session_offset: time["session_offset"].as_f64(),
            video_info,
            audio,
            hz: raw["loudness"]["hz"]
                .as_u64()
                .filter(|&hz| hz > 0)
                .map_or(DEFAULT_HZ, |hz| hz as u32),
            events,
            analysis: raw.get("analysis").filter(|a| a.is_object()).cloned(),
            starred: raw["starred"].as_bool().unwrap_or(false),
            raw,
        })
    }

    /// A clip known only from its session journal line.
    #[allow(clippy::too_many_arguments)]
    pub fn from_journal(
        id: &str,
        video: &Path,
        session: &str,
        from: f64,
        to: f64,
        trigger_t: Option<f64>,
        game: Game,
        session_start_utc: Option<f64>,
        utc_offset_min: Option<i32>,
        markers: Vec<Event>,
    ) -> Self {
        let file = video
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let start_utc = session_start_utc.map(|s| s + from);
        let duration = (to - from).max(0.0);
        let events_json: Vec<Value> = markers.iter().map(|m| m.raw.clone()).collect();
        let raw = json!({
            "rec": SIDECAR_VERSION,
            "source": "journal",
            "clip": {
                "id": id,
                "file": file,
                "parent": null,
                "duration": round3(duration),
                "saved_at": trigger_t.map(|t| round3(t - from)),
            },
            "game": {
                "name": game.name,
                "exe": game.exe,
                "steam_appid": game.steam_appid,
            },
            "time": {
                "start_utc": start_utc.map(format_utc),
                "utc_offset_min": utc_offset_min,
                "session": session,
                "session_offset": round3(from),
            },
            "audio": [],
            "events": events_json,
        });
        Self {
            source: Source::Journal,
            id: id.to_owned(),
            file,
            video: Some(video.to_owned()),
            sidecar: None,
            parent: None,
            duration,
            trigger: None,
            saved_at: trigger_t.map(|t| t - from),
            game,
            start_utc,
            utc_offset_min,
            session: Some(session.to_owned()),
            session_offset: Some(from),
            video_info: None,
            audio: Vec::new(),
            hz: DEFAULT_HZ,
            events: markers,
            analysis: None,
            starred: false,
            raw,
        }
    }

    /// The clip's span in its game session: `(session, from, to)` seconds.
    pub fn session_range(&self) -> Option<(&str, f64, f64)> {
        let from = self.session_offset?;
        Some((self.session.as_deref()?, from, from + self.duration))
    }

    /// The video's file name without extension, for naming outputs.
    pub fn stem(&self) -> String {
        let name = self
            .video
            .as_deref()
            .and_then(Path::file_stem)
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.file.clone());
        match name.rsplit_once('.') {
            Some((stem, ext)) if VIDEO_EXTENSIONS.contains(&ext) => stem.to_owned(),
            _ => name,
        }
    }

    /// Number of events of each kind.
    pub fn event_counts(&self) -> Map<String, Value> {
        let mut counts = Map::new();
        for e in &self.events {
            let n = counts.get(&e.kind).and_then(Value::as_u64).unwrap_or(0);
            counts.insert(e.kind.clone(), (n + 1).into());
        }
        counts
    }

    /// The best track for game sound: `game`, else `mix`, else `system`.
    pub fn game_track(&self) -> Option<&Track> {
        ["game", "mix", "system"]
            .iter()
            .find_map(|role| self.audio.iter().find(|t| t.role == *role))
    }

    /// A track by index (`"1"`) or role (`"mic"`).
    pub fn track(&self, key: &str) -> Option<&Track> {
        match key.parse::<usize>() {
            Ok(i) => self.audio.iter().find(|t| t.index == i),
            Err(_) => self.audio.iter().find(|t| t.role == key || t.name == key),
        }
    }
}

impl Event {
    pub fn from_value(v: &Value) -> Option<Self> {
        Some(Self {
            t: v["t"].as_f64()?,
            kind: str_of(&v["kind"])?,
            src: str_of(&v["src"]).unwrap_or_default(),
            conf: v["conf"].as_f64().unwrap_or(1.0),
            dur: v["dur"].as_f64(),
            raw: v.clone(),
        })
    }
}

/// Stars or unstars a clip; quota pruning never deletes a starred clip. The
/// star is kept in the sidecar, read afresh so nothing written since the
/// clip was loaded (analysis) is lost. A clip known only from the journal
/// gets a sidecar built from it: the star has to be kept somewhere.
pub fn set_starred(clip: &mut Clip, starred: bool) -> Result<(), String> {
    let (path, mut raw) = match &clip.sidecar {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("can't read {}: {e}", path.display()))?;
            let raw: Value = serde_json::from_str(&text)
                .map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
            if !raw.is_object() || raw.get("rec").is_none() {
                return Err(format!("{} is not a rec sidecar", path.display()));
            }
            (path.clone(), raw)
        }
        None if !starred => {
            clip.starred = false;
            return Ok(());
        }
        None => {
            let video = (clip.video.as_deref()).ok_or("the clip's video is missing")?;
            let path = sidecar_for(video);
            if path.exists() {
                return Err(format!(
                    "{} exists but isn't a readable rec sidecar",
                    path.display()
                ));
            }
            let mut raw = clip.raw.clone();
            if let Some(fields) = raw.as_object_mut() {
                fields.shift_remove("source");
            }
            (path, raw)
        }
    };
    raw["starred"] = starred.into();
    write_sidecar(&path, &raw)?;
    clip.raw = raw;
    clip.starred = starred;
    clip.sidecar = Some(path);
    clip.source = Source::Sidecar;
    Ok(())
}

/// Writes a sidecar, replacing the old file in one step.
pub fn write_sidecar(path: &Path, raw: &Value) -> Result<(), String> {
    let mut text = serde_json::to_string_pretty(raw).map_err(|e| e.to_string())?;
    text = crate::analysis::compact_number_arrays(&text);
    text.push('\n');
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("can't write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("can't replace {}: {e}", path.display())
    })
}

fn str_of(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}

/// Rounds to the millisecond, as recd writes times.
pub fn round3(t: f64) -> f64 {
    (t * 1000.0).round() / 1000.0
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A sidecar as phase A writes it.
    pub fn sample() -> Value {
        json!({
            "rec": 1,
            "clip": {
                "id": "01TESTCLIP0000000000000000",
                "file": "Team Fortress 2_2026-10-17_21-04-33.mp4",
                "parent": null,
                "duration": 30.0,
                "trigger": "hotkey",
                "saved_at": 27.5
            },
            "game": { "name": "Team Fortress 2", "exe": "tf_win64.exe", "steam_appid": 440 },
            "time": {
                "start_utc": "2026-10-18T01:04:03.112Z",
                "utc_offset_min": -240,
                "session": "01TESTSESSION0000000000000",
                "session_offset": 1843.226
            },
            "video": { "codec": "hevc", "width": 2560, "height": 1440, "fps": 60, "hdr": false },
            "audio": [
                { "index": 0, "name": "Mixed", "role": "mix", "loudness": [0, 10, 20] },
                { "index": 1, "name": "Game", "role": "game", "loudness": [0, 255, 300] },
                { "index": 2, "name": "Microphone", "role": "mic", "loudness": [] }
            ],
            "events": [
                { "t": 24.9, "kind": "marker", "src": "hotkey", "conf": 1 },
                { "t": 21.18, "kind": "marker", "src": "cli", "conf": 1 },
                { "t": 3.0, "kind": "tf2:uber", "src": "source:tf2-console", "conf": 1, "actor": "me" },
                { "kind": "broken" }
            ],
            "loudness": { "hz": 10, "scale": "RMS level" },
            "future_field": { "kept": true }
        })
    }

    #[test]
    fn reads_a_phase_a_sidecar() {
        let clip = Clip::from_value(sample()).unwrap();
        assert_eq!(clip.id, "01TESTCLIP0000000000000000");
        assert_eq!(clip.duration, 30.0);
        assert_eq!(clip.saved_at, Some(27.5));
        assert_eq!(clip.game.steam_appid, Some(440));
        assert_eq!(clip.utc_offset_min, Some(-240));
        assert_eq!(
            clip.start_utc.map(format_utc).as_deref(),
            Some("2026-10-18T01:04:03.112Z")
        );
        assert_eq!(
            clip.session_range(),
            Some(("01TESTSESSION0000000000000", 1843.226, 1873.226))
        );
        assert_eq!(clip.video_info.as_ref().unwrap().width, 2560);
        assert_eq!(clip.audio[1].loudness, vec![0, 255, 255]);
        assert_eq!(clip.game_track().unwrap().role, "game");
        assert_eq!(clip.track("mic").unwrap().index, 2);
        assert_eq!(clip.track("1").unwrap().name, "Game");
        // Sorted, the broken one dropped, unknown fields kept.
        let times: Vec<f64> = clip.events.iter().map(|e| e.t).collect();
        assert_eq!(times, vec![3.0, 21.18, 24.9]);
        assert_eq!(clip.events[0].raw["actor"], "me");
        assert_eq!(clip.event_counts()["marker"], 2);
        assert_eq!(clip.raw["future_field"]["kept"], true);
        assert_eq!(clip.stem(), "Team Fortress 2_2026-10-17_21-04-33");
    }

    #[test]
    fn rejects_other_json() {
        assert!(Clip::from_value(json!({ "clip": { "id": "x" } })).is_err());
        assert!(Clip::from_value(json!({ "rec": 1, "clip": {} })).is_err());
    }

    #[test]
    fn sidecar_names() {
        assert_eq!(
            sidecar_for(Path::new(r"C:\c\TF2 (2).mp4")),
            Path::new(r"C:\c\TF2 (2).rec.json")
        );
        assert_eq!(
            sidecar_stem(Path::new(r"C:\c\TF2 (2).rec.json")).as_deref(),
            Some("TF2 (2)")
        );
        assert_eq!(sidecar_stem(Path::new(r"C:\c\.rec.json")), None);
        assert_eq!(sidecar_stem(Path::new(r"C:\c\a.json")), None);
    }

    #[test]
    fn journal_clip_has_sidecar_shape() {
        let marker =
            Event::from_value(&json!({"t": 2.5, "kind": "marker", "src": "hotkey", "conf": 1}))
                .unwrap();
        let clip = Clip::from_journal(
            "01JOURNALCLIP",
            Path::new(r"C:\clips\a.mp4"),
            "01SESSION",
            100.0,
            130.0,
            Some(128.0),
            Game {
                name: "Game".into(),
                exe: "game.exe".into(),
                steam_appid: None,
            },
            Some(1000.0),
            Some(60),
            vec![marker],
        );
        assert_eq!(clip.source, Source::Journal);
        assert_eq!(clip.duration, 30.0);
        assert_eq!(clip.saved_at, Some(28.0));
        assert_eq!(clip.start_utc, Some(1100.0));
        let reread = Clip::from_value(clip.raw.clone()).unwrap();
        assert_eq!(reread.session_range(), Some(("01SESSION", 100.0, 130.0)));
        assert_eq!(reread.events.len(), 1);
        assert_eq!(reread.raw["source"], "journal");
    }
}
