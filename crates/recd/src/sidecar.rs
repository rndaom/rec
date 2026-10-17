//! The clip sidecar: `<clip>.rec.json` next to every saved clip. It says
//! what an AI (or any tool) needs to understand the clip without decoding it:
//! the game, when the clip was recorded and where it sits in the game
//! session, which audio track is which, markers in clip time, and a loudness
//! curve per audio track. Format and reasons: `docs/ai-clips.md`.
//!
//! Times in the file are seconds from the clip's first frame (its t = 0),
//! rounded to the millisecond, unless named otherwise.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rec_audio::levels::{LEVEL_HZ, Level, Levels};
use rec_core::media::{TrackInfo, TrackKind, VIDEO_TRACK, VideoFormat};
use rec_core::replay::Packet;
use rec_core::time::{HNS_PER_SEC, Hns};
use serde::Serialize;
use serde_json::value::RawValue;

use crate::journal::{SessionInfo, format_utc, new_id, round_ms, utc_at, utc_offset_minutes};

/// Version of the sidecar format. Readers should accept unknown fields;
/// this goes up only when an existing field changes meaning.
pub const VERSION: u32 = 1;

/// What asked for the clip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    Hotkey,
    Panel,
    Tray,
    /// `rec save`, or another program over IPC.
    Cli,
}

impl Trigger {
    fn name(self) -> &'static str {
        match self {
            Self::Hotkey => "hotkey",
            Self::Panel => "panel",
            Self::Tray => "tray",
            Self::Cli => "cli",
        }
    }
}

/// What the save thread needs, besides the packets, to describe a clip.
#[derive(Debug)]
pub struct ClipContext {
    pub trigger: Trigger,
    /// QPC time the save was requested.
    pub requested: Hns,
    pub session: Arc<SessionInfo>,
    pub levels: Option<Levels>,
    /// Write `<clip>.rec.json` (`output.sidecar`); the journal line is
    /// written either way.
    pub sidecar: bool,
}

/// A saved clip's place in time, for the journal.
#[derive(Debug)]
pub struct Saved {
    pub id: String,
    /// QPC time of the clip's first frame and of its end.
    pub start: Hns,
    pub end: Hns,
}

/// The clip's first frame and end on the QPC clock, from its video packets.
pub fn clip_range(packets: &[Packet]) -> Option<(Hns, Hns)> {
    let mut video = packets.iter().filter(|p| p.track == VIDEO_TRACK);
    let start = video.next()?.pts;
    let end = (packets.iter())
        .filter(|p| p.track == VIDEO_TRACK)
        .map(|p| p.pts + p.duration)
        .max()?;
    Some((start, end))
}

/// `<clip>.rec.json` for `clip.mp4`.
pub fn path_for(clip: &Path) -> PathBuf {
    clip.with_extension("rec.json")
}

/// Describes the clip at `clip` and writes its sidecar if enabled.
pub fn describe(
    clip: &Path,
    ctx: &ClipContext,
    tracks: &[TrackInfo],
    format: &VideoFormat,
    packets: &[Packet],
) -> Result<Saved, String> {
    let (start, end) = clip_range(packets).ok_or("the clip has no video")?;
    let saved = Saved {
        id: new_id(),
        start,
        end,
    };
    if ctx.sidecar {
        let json = render(clip, ctx, &saved, tracks, format)?;
        let path = path_for(clip);
        std::fs::write(&path, json).map_err(|e| format!("can't write {}: {e}", path.display()))?;
    }
    Ok(saved)
}

#[derive(Serialize)]
struct Sidecar<'a> {
    rec: u32,
    clip: Clip<'a>,
    game: Game<'a>,
    time: Time,
    video: Video,
    audio: Vec<Audio<'a>>,
    events: Vec<Event>,
    loudness: Loudness,
}

#[derive(Serialize)]
struct Clip<'a> {
    id: &'a str,
    file: String,
    /// Id of the clip this one was cut from (trims and exports, later).
    parent: Option<&'a str>,
    duration: f64,
    trigger: &'static str,
    /// When the save was asked for. Clips include post-roll, so this can be
    /// before the end.
    saved_at: f64,
}

#[derive(Serialize)]
struct Game<'a> {
    name: &'a str,
    exe: &'a str,
    steam_appid: Option<u32>,
}

#[derive(Serialize)]
struct Time {
    /// Wall clock of t = 0.
    start_utc: String,
    /// Local time minus UTC when the clip was saved.
    utc_offset_min: i32,
    session: String,
    /// Where t = 0 is in the session, in seconds from its start.
    session_offset: f64,
}

#[derive(Serialize)]
struct Video {
    codec: String,
    width: u32,
    height: u32,
    fps: u32,
    hdr: bool,
}

#[derive(Serialize)]
struct Audio<'a> {
    /// Index among the clip's audio streams (`0:a:<index>` in ffmpeg).
    index: usize,
    name: &'a str,
    /// One of `mix`, `game`, `voice`, `other`, `mic`, `system`.
    role: &'static str,
    /// One value per `loudness.hz` from t = 0; see `loudness.scale`.
    loudness: Box<RawValue>,
}

#[derive(Serialize)]
struct Event {
    t: f64,
    kind: &'static str,
    src: &'static str,
    /// 1 for events rec knows for certain; analysis adds guesses below 1.
    conf: f64,
}

#[derive(Serialize)]
struct Loudness {
    hz: u32,
    scale: &'static str,
}

fn render(
    clip: &Path,
    ctx: &ClipContext,
    saved: &Saved,
    tracks: &[TrackInfo],
    format: &VideoFormat,
) -> Result<String, String> {
    let session = &ctx.session;
    let duration = saved.end - saved.start;
    let samples =
        (duration.max(0) as u64 * u64::from(LEVEL_HZ)).div_ceil(HNS_PER_SEC as u64) as usize;
    let audio = tracks
        .iter()
        .filter(|t| matches!(t.kind, TrackKind::Audio(_)))
        .enumerate()
        .map(|(index, track)| {
            let levels = ctx
                .levels
                .as_ref()
                .map_or_else(Vec::new, |l| l.range(track.id, saved.start, saved.end));
            let curve = resample(&levels, saved.start, samples);
            Ok(Audio {
                index,
                name: &track.name,
                role: role(&track.name),
                loudness: compact(&curve)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let events = session
        .markers(saved.start, saved.end)
        .into_iter()
        .map(|m| Event {
            t: round_ms(m.t - saved.start),
            kind: "marker",
            src: m.src.name(),
            conf: 1.0,
        })
        .collect();
    let sidecar = Sidecar {
        rec: VERSION,
        clip: Clip {
            id: &saved.id,
            file: clip
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            parent: None,
            duration: round_ms(duration),
            trigger: ctx.trigger.name(),
            saved_at: round_ms(ctx.requested - saved.start),
        },
        game: Game {
            name: &session.game,
            exe: &session.exe_name,
            steam_appid: session.steam_appid,
        },
        time: Time {
            start_utc: format_utc(utc_at(saved.start)),
            utc_offset_min: utc_offset_minutes(),
            session: session.id.clone(),
            session_offset: session.offset(saved.start),
        },
        video: Video {
            codec: format.codec.name().to_ascii_lowercase().replace('.', ""),
            width: format.width,
            height: format.height,
            fps: format.fps,
            hdr: format.color.is_hdr(),
        },
        audio,
        events,
        loudness: Loudness {
            hz: LEVEL_HZ,
            scale: "RMS level: 0 is -60 dBFS or quieter, 255 is 0 dBFS, linear in dB",
        },
    };
    let mut json = serde_json::to_string_pretty(&sidecar).map_err(|e| e.to_string())?;
    json.push('\n');
    Ok(json)
}

/// A number array on one line, so the pretty-printed file stays readable.
fn compact(values: &[u8]) -> Result<Box<RawValue>, String> {
    let text = serde_json::to_string(values).map_err(|e| e.to_string())?;
    RawValue::from_string(text).map_err(|e| e.to_string())
}

/// The audio role of a track, from the names `session::plan_audio` gives.
fn role(name: &str) -> &'static str {
    match name {
        "Mixed" => "mix",
        "Game" => "game",
        "Voice chat" => "voice",
        "Other apps" => "other",
        "Microphone" => "mic",
        _ => "system",
    }
}

/// `n` values at `LEVEL_HZ` from `start`: each is the level of the window
/// holding the middle of its slot, 0 where there is none.
fn resample(levels: &[Level], start: Hns, n: usize) -> Vec<u8> {
    let step = HNS_PER_SEC / i64::from(LEVEL_HZ);
    let mut next = 0;
    (0..n as i64)
        .map(|k| {
            let at = start + k * step + step / 2;
            while next + 1 < levels.len() && levels[next + 1].t <= at {
                next += 1;
            }
            levels
                .get(next)
                .filter(|l| (l.t..l.t + step).contains(&at))
                .map_or(0, |l| l.value)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rec_core::time::millis;

    fn level(t: Hns, value: u8) -> Level {
        Level { t, value }
    }

    #[test]
    fn resample_aligns_windows_to_clip_time() {
        // Windows every 100 ms starting 30 ms before the clip.
        let start = 1_000_000;
        let levels: Vec<_> = (0..5)
            .map(|i| level(start - millis(30) + i * millis(100), 10 * (i as u8 + 1)))
            .collect();
        // Slot k's middle is at start + 50 + 100k ms, inside window k.
        assert_eq!(resample(&levels, start, 4), vec![10, 20, 30, 40]);
        // Past the last window: 0.
        assert_eq!(resample(&levels, start, 7)[5..], [0, 0]);
    }

    #[test]
    fn resample_fills_gaps_with_zero() {
        let levels = [level(0, 50), level(millis(300), 60)];
        assert_eq!(resample(&levels, 0, 4), vec![50, 0, 0, 60]);
        assert_eq!(resample(&[], 0, 3), vec![0, 0, 0]);
    }

    #[test]
    fn roles() {
        assert_eq!(role("Mixed"), "mix");
        assert_eq!(role("Microphone"), "mic");
        assert_eq!(role("Game & system audio"), "system");
    }

    #[test]
    fn sidecar_path() {
        assert_eq!(
            path_for(Path::new(r"C:\clips\TF2_2026 (2).mp4")),
            Path::new(r"C:\clips\TF2_2026 (2).rec.json")
        );
    }

    fn packet(track: u8, pts: Hns) -> Packet {
        Packet {
            track,
            segment: 0,
            pts,
            dts: pts,
            duration: 10,
            keyframe: true,
            data: Arc::from(&[0u8][..]),
        }
    }

    #[test]
    fn renders_a_complete_sidecar() {
        use crate::journal::{MarkSource, Marker};
        use rec_core::media::{AudioCodec, AudioFormat, VideoCodec, VideoColor};

        let start = rec_core::time::secs(100);
        let session = SessionInfo::for_test(
            start - rec_core::time::secs(60),
            vec![
                // Before the clip, inside it, after it.
                Marker {
                    t: start - millis(1),
                    src: MarkSource::Hotkey,
                },
                Marker {
                    t: start + millis(1500),
                    src: MarkSource::Cli,
                },
                Marker {
                    t: start + millis(2001),
                    src: MarkSource::Hotkey,
                },
            ],
        );
        let ctx = ClipContext {
            trigger: Trigger::Hotkey,
            requested: start + millis(1800),
            session: Arc::new(session),
            levels: None,
            sidecar: true,
        };
        let format = VideoFormat {
            codec: VideoCodec::Hevc,
            width: 1920,
            height: 1080,
            fps: 60,
            color: VideoColor::Sdr,
        };
        let audio = AudioFormat {
            codec: AudioCodec::Aac,
            sample_rate: 48_000,
            channels: 2,
            codec_config: vec![0x11, 0x90],
        };
        let tracks = vec![
            TrackInfo {
                id: 0,
                name: "Video".into(),
                kind: TrackKind::Video(format.clone()),
            },
            TrackInfo {
                id: 1,
                name: "Mixed".into(),
                kind: TrackKind::Audio(audio.clone()),
            },
            TrackInfo {
                id: 2,
                name: "Game".into(),
                kind: TrackKind::Audio(audio),
            },
        ];
        let saved = Saved {
            id: "01TESTCLIP0000000000000000".into(),
            start,
            end: start + millis(2000),
        };
        let json = render(
            Path::new(r"C:\clips\tf2.mp4"),
            &ctx,
            &saved,
            &tracks,
            &format,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["rec"], 1);
        assert_eq!(v["clip"]["file"], "tf2.mp4");
        assert_eq!(v["clip"]["duration"], 2.0);
        assert_eq!(v["clip"]["saved_at"], 1.8);
        assert_eq!(v["clip"]["trigger"], "hotkey");
        assert_eq!(v["game"]["steam_appid"], 440);
        assert_eq!(v["time"]["session_offset"], 60.0);
        assert_eq!(v["video"]["codec"], "hevc");
        assert_eq!(v["audio"][1]["index"], 1);
        assert_eq!(v["audio"][1]["role"], "game");
        assert_eq!(v["audio"][0]["loudness"].as_array().unwrap().len(), 20);
        let events = v["events"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["t"], 1.5);
        assert_eq!(events[0]["src"], "cli");
        // Curves stay on one line.
        assert!(json.contains("\"loudness\": [0,0,"), "{json}");
    }

    #[test]
    fn range_comes_from_video() {
        let packets = [packet(1, 0), packet(0, 5), packet(1, 12), packet(0, 15)];
        assert_eq!(clip_range(&packets), Some((5, 25)));
        assert_eq!(clip_range(&[packet(1, 0)]), None);
    }
}
