//! Game sessions: one continuous run of a game, from the first time rec
//! records it until it closes. A session outlives its recording pipelines (a
//! settings reload, a pause or a failed start keeps it), so its markers and
//! clips share one timeline. Each session appends JSON lines to
//! `%LOCALAPPDATA%\rec\sessions\<id>.jsonl`: when it started, every marker,
//! every clip saved with its range, and when it ended. Clip ranges on one
//! timeline make overlapping clips easy to spot (see `docs/ai-clips.md`).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use rec_core::config::MAX_REPLAY_SECONDS;
use rec_core::time::{HNS_PER_MS, HNS_PER_SEC, Hns, as_secs_f64, qpc_now, secs};
use serde_json::{Value, json};
use windows::Win32::Foundation::FILETIME;
use windows::Win32::Storage::FileSystem::FileTimeToLocalFileTime;

use crate::log::warning;
use crate::session::Target;
use crate::sidecar::ClipProfile;

/// Markers older than this can't be in any clip and are dropped.
const MARKER_HISTORY: Hns = secs(MAX_REPLAY_SECONDS as i64 + 60);

/// What added a marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkSource {
    Hotkey,
    /// A controller combo (`hotkeys.controller_add_marker`).
    Controller,
    /// `rec mark`, or another program over IPC.
    Cli,
}

impl MarkSource {
    pub fn name(self) -> &'static str {
        match self {
            Self::Hotkey => "hotkey",
            Self::Controller => "controller",
            Self::Cli => "cli",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Marker {
    /// QPC time.
    pub t: Hns,
    pub src: MarkSource,
}

/// The parts of a game session the save thread reads.
#[derive(Debug)]
pub struct SessionInfo {
    pub id: String,
    pub game: String,
    pub exe_name: String,
    pub steam_appid: Option<u32>,
    /// QPC time the session started.
    pub start: Hns,
    /// `None` when the sessions folder could not be created.
    pub journal: Option<PathBuf>,
    markers: Mutex<Vec<Marker>>,
}

impl SessionInfo {
    /// Markers in `[from, to]`, oldest first.
    pub fn markers(&self, from: Hns, to: Hns) -> Vec<Marker> {
        let markers = self.markers.lock().unwrap_or_else(PoisonError::into_inner);
        markers
            .iter()
            .filter(|m| (from..=to).contains(&m.t))
            .copied()
            .collect()
    }

    /// Seconds since the session started, rounded to the millisecond.
    pub fn offset(&self, t: Hns) -> f64 {
        round_ms(t - self.start)
    }

    /// Appends one line to the journal. Failures are logged once per call
    /// and otherwise ignored: the journal is a convenience.
    pub fn append(&self, mut line: Value) {
        let Some(path) = &self.journal else {
            return;
        };
        if let Value::Object(fields) = &mut line {
            fields.insert("utc".into(), format_utc(utc_at(qpc_now())).into());
        }
        let mut text = line.to_string();
        text.push('\n');
        // One write per line, in append mode: lines from the main and the save
        // thread never interleave.
        let written = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| f.write_all(text.as_bytes()));
        if let Err(e) = written {
            warning!("could not write {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
impl SessionInfo {
    /// A session without a journal that started at `start`.
    pub fn for_test(start: Hns, markers: Vec<Marker>) -> Self {
        Self {
            id: "01TESTSESSION0000000000000".into(),
            game: "Team Fortress 2".into(),
            exe_name: "tf_win64.exe".into(),
            steam_appid: Some(440),
            start,
            journal: None,
            markers: Mutex::new(markers),
        }
    }
}

/// A running game session.
#[derive(Debug)]
pub struct GameSession {
    info: Arc<SessionInfo>,
    pid: u32,
    exe_path: PathBuf,
}

impl GameSession {
    /// Starts a session for `target` and writes its first journal line, with
    /// the game's Steam app id ([`steam_appid`]) and profile.
    pub fn start(
        target: &Target,
        data_dir: &Path,
        steam_appid: Option<u32>,
        profile: Option<&ClipProfile>,
    ) -> Self {
        let dir = data_dir.join("sessions");
        let id = new_id();
        let journal = match std::fs::create_dir_all(&dir) {
            Ok(()) => Some(dir.join(format!("{id}.jsonl"))),
            Err(e) => {
                warning!("no session journal: can't create {}: {e}", dir.display());
                None
            }
        };
        let info = Arc::new(SessionInfo {
            id,
            game: target.game.clone(),
            exe_name: target.exe_name.clone(),
            steam_appid,
            start: qpc_now(),
            journal,
            markers: Mutex::new(Vec::new()),
        });
        info.append(json!({
            "type": "start",
            "session": info.id,
            "game": info.game,
            "exe": info.exe_name,
            "steam_appid": info.steam_appid,
            "profile": profile.map(|p| &p.id),
            "player_names": profile.map_or(&[][..], |p| &p.player_names),
            "utc_offset_min": utc_offset_minutes(),
        }));
        Self {
            info,
            pid: target.pid,
            exe_path: target.exe_path.clone(),
        }
    }

    /// Whether `target` is the same run of the same game (a new window of it
    /// included), so it continues this session.
    pub fn continues(&self, target: &Target) -> bool {
        self.pid == target.pid && self.exe_path == target.exe_path
    }

    pub fn info(&self) -> Arc<SessionInfo> {
        Arc::clone(&self.info)
    }

    /// Adds a marker at `t` (QPC time).
    pub fn mark(&self, t: Hns, src: MarkSource) {
        {
            let mut markers = self
                .info
                .markers
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            markers.retain(|m| t - m.t <= MARKER_HISTORY);
            markers.push(Marker { t, src });
        }
        self.info.append(json!({
            "type": "marker",
            "t": self.info.offset(t),
            "src": src.name(),
        }));
    }

    /// Writes the last journal line.
    pub fn end(self, why: &str) {
        self.info.append(json!({
            "type": "end",
            "t": self.info.offset(qpc_now()),
            "why": why,
        }));
    }
}

/// `t` in seconds, rounded to the millisecond.
pub fn round_ms(t: Hns) -> f64 {
    as_secs_f64(t / HNS_PER_MS * HNS_PER_MS)
}

// ---- time ---------------------------------------------------------------------

/// Unix time in 100 ns ticks of QPC time `t`. The two clocks are read
/// together at each call, so drift between them never builds up.
pub fn utc_at(t: Hns) -> i64 {
    let now_qpc = qpc_now();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| (d.as_nanos() / 100) as i64);
    now - (now_qpc - t)
}

/// E.g. `2026-10-17T21:04:03.112Z`.
pub fn format_utc(unix: i64) -> String {
    let ms = unix.div_euclid(HNS_PER_MS);
    let (days, ms_of_day) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let (y, m, d) = civil_from_days(days);
    let s = ms_of_day / 1000;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        s / 3600,
        s / 60 % 60,
        s % 60,
        ms_of_day % 1000
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Minutes local time is ahead of UTC right now (e.g. -240 for EDT).
pub fn utc_offset_minutes() -> i32 {
    // 2001-01-01 as a FILETIME (100 ns since 1601): any fixed instant works,
    // the bias applied is today's.
    let utc: i64 = 126_227_808_000_000_000;
    let ft = FILETIME {
        dwLowDateTime: utc as u32,
        dwHighDateTime: (utc >> 32) as u32,
    };
    let mut local = FILETIME::default();
    // SAFETY: both pointers are valid FILETIMEs.
    if unsafe { FileTimeToLocalFileTime(&ft, &mut local) }.is_err() {
        return 0;
    }
    let local = (i64::from(local.dwHighDateTime) << 32) | i64::from(local.dwLowDateTime);
    ((local - utc) / (60 * HNS_PER_SEC)) as i32
}

// ---- ids ----------------------------------------------------------------------

pub use rec_core::store::new_id;

// ---- Steam --------------------------------------------------------------------

/// The Steam app id of a game installed by Steam, from its exe path
/// (`...\steamapps\common\<installdir>\...`) and the library's
/// `appmanifest_<id>.acf` files.
pub fn steam_appid(exe_path: &Path) -> Option<u32> {
    let parts: Vec<_> = exe_path.components().collect();
    let common = parts.windows(2).position(|w| {
        matches!(w, [Component::Normal(a), Component::Normal(b)]
            if a.eq_ignore_ascii_case("steamapps") && b.eq_ignore_ascii_case("common"))
    })?;
    let Component::Normal(installdir) = parts.get(common + 2)? else {
        return None;
    };
    let installdir = installdir.to_str()?;
    let steamapps: PathBuf = parts[..=common].iter().collect();
    std::fs::read_dir(&steamapps)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("appmanifest_") && name.ends_with(".acf")
        })
        .find_map(|e| {
            let text = std::fs::read_to_string(e.path()).ok()?;
            let dir = acf_value(&text, "installdir")?;
            dir.eq_ignore_ascii_case(installdir)
                .then(|| acf_value(&text, "appid")?.parse().ok())?
        })
}

/// The first `"key"  "value"` pair's value in a Valve KeyValues file.
fn acf_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let mut quoted = line.split('"').skip(1).step_by(2);
        (quoted.next()?.eq_ignore_ascii_case(key))
            .then(|| quoted.next())
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00.000Z");
        // 2026-10-17T21:04:03.112Z
        let t = (1_792_271_043_112 * HNS_PER_MS) + 9_999;
        assert_eq!(format_utc(t), "2026-10-17T21:04:03.112Z");
        // 2000-02-29 and the day after (leap day of a century leap year).
        assert_eq!(
            format_utc(951_782_400_000 * HNS_PER_MS),
            "2000-02-29T00:00:00.000Z"
        );
        assert_eq!(
            format_utc(951_868_800_000 * HNS_PER_MS),
            "2000-03-01T00:00:00.000Z"
        );
        assert_eq!(format_utc(-HNS_PER_MS), "1969-12-31T23:59:59.999Z");
    }

    #[test]
    fn utc_at_maps_qpc_to_now() {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let now = (now.as_nanos() / 100) as i64;
        let t = utc_at(qpc_now() - secs(10));
        assert!((now - secs(10) - t).abs() < secs(1));
    }

    #[test]
    fn acf_values() {
        let text =
            "\"AppState\"\n{\n\t\"appid\"\t\t\"440\"\n\t\"installdir\"\t\t\"Team Fortress 2\"\n}\n";
        assert_eq!(acf_value(text, "appid"), Some("440"));
        assert_eq!(acf_value(text, "installdir"), Some("Team Fortress 2"));
        assert_eq!(acf_value(text, "name"), None);
    }

    #[test]
    fn steam_appid_from_library() {
        let root = std::env::temp_dir().join(format!("rec-journal-test-{}", std::process::id()));
        let steamapps = root.join("SteamLibrary").join("steamapps");
        std::fs::create_dir_all(steamapps.join("common").join("Team Fortress 2")).unwrap();
        std::fs::write(
            steamapps.join("appmanifest_440.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\t\"440\"\n\t\"installdir\"\t\t\"Team Fortress 2\"\n}\n",
        )
        .unwrap();
        std::fs::write(
            steamapps.join("appmanifest_730.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\t\"730\"\n\t\"installdir\"\t\t\"Counter-Strike Global Offensive\"\n}\n",
        )
        .unwrap();
        let exe = steamapps
            .join("common")
            .join("team fortress 2")
            .join("tf_win64.exe");
        assert_eq!(steam_appid(&exe), Some(440));
        let other = steamapps.join("common").join("Unknown").join("game.exe");
        assert_eq!(steam_appid(&other), None);
        assert_eq!(steam_appid(Path::new(r"C:\Games\tf_win64.exe")), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn utc_offset_is_plausible() {
        assert!((-14 * 60..=14 * 60).contains(&utc_offset_minutes()));
    }
}
