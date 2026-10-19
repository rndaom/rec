//! Per-game profiles: one plain TOML file per game in
//! `%APPDATA%\rec\profiles\<id>.toml`, which users can edit and share as they
//! are (decision D16).
//!
//! A profile names its game (`[game]`: executables and, when known, the
//! Steam app id), the player's names in it (`[player]`, written into clip
//! sidecars), and may set some `[replay]`, `[video]` and `[audio]` settings
//! (see [`OVERRIDABLE`]). While rec records the game, those replace the same
//! settings in `config.toml`; a setting the profile leaves out follows
//! `config.toml`. `0` or `""` set an optional setting back to "unset" (see
//! `Config::normalize`), so a profile can undo one the config file sets.
//!
//! `[hud]` and `[sources]` are reserved for game event sources and HUD
//! readers (`docs/ai-clips.md`, phase D). Anything else is ignored and
//! reported, never an error, so a profile from a newer rec still loads.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::{self, Config, ConfigError};

/// The settings a profile may set, by section. Everything else is global:
/// where clips go and how much space they take, hotkeys and controller
/// combos (the same keys must not do different things from game to game),
/// what counts as a game, and the capture target, which is chosen before
/// the game's profile is known.
pub const OVERRIDABLE: &[(&str, &[&str])] = &[
    ("replay", &["seconds", "post_roll_seconds", "max_memory_mb"]),
    (
        "video",
        &[
            "capture",
            "fps",
            "max_height",
            "codec",
            "quality",
            "bitrate_kbps",
            "encoder",
            "cursor",
            "hdr",
            "gpu_priority",
        ],
    ),
    (
        "audio",
        &[
            "game",
            "voice_chat",
            "voice_chat_apps",
            "microphone",
            "microphone_device",
            "other_apps",
            "mixed_track",
            "bitrate_kbps",
        ],
    ),
];

/// Sections that belong to the profile itself, not to the config.
const OWN_SECTIONS: &[&str] = &["game", "player", "hud", "sources"];

/// Whether a profile may set dotted `"section.key"`.
pub fn overridable(dotted: &str) -> bool {
    dotted.split_once('.').is_some_and(|(section, key)| {
        OVERRIDABLE
            .iter()
            .any(|(s, keys)| *s == section && keys.contains(&key))
    })
}

/// The value that writes "unset" for an optional setting, which a profile
/// needs to undo a value `config.toml` sets.
fn unset_value(dotted: &str) -> Option<toml::Value> {
    match dotted {
        "video.bitrate_kbps" | "video.max_height" => Some(toml::Value::Integer(0)),
        "audio.microphone_device" => Some(toml::Value::String(String::new())),
        _ => None,
    }
}

/// `%APPDATA%\rec\profiles`, next to the config file.
pub fn default_dir() -> PathBuf {
    let config = config::default_config_path();
    config
        .parent()
        .map_or_else(|| PathBuf::from("profiles"), |dir| dir.join("profiles"))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    /// The file name without `.toml`. Written into clips as `game.profile`.
    pub id: String,
    pub path: PathBuf,
    /// The game's friendly name, as rec showed it when it made the file.
    pub name: Option<String>,
    /// Executable file names this profile is for (compared ignoring case).
    pub exe: Vec<String>,
    pub steam_appid: Option<u32>,
    /// The player's names in this game, for clip sidecars (`player.names`).
    pub player_names: Vec<String>,
    /// The settings it sets: section -> key -> value, only [`OVERRIDABLE`]
    /// ones.
    overrides: toml::Table,
    /// Dotted names of settings or sections in the file that a profile
    /// can't set; they are ignored.
    pub ignored: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct GameSection {
    name: Option<String>,
    exe: Exe,
    steam_appid: Option<u32>,
}

/// `exe = "game.exe"` or `exe = ["game.exe", "game_dx12.exe"]`.
#[derive(Deserialize)]
#[serde(untagged)]
enum Exe {
    One(String),
    Many(Vec<String>),
}

impl Default for Exe {
    fn default() -> Self {
        Self::Many(Vec::new())
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct PlayerSection {
    names: Vec<String>,
}

impl Profile {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(path, &text)
    }

    pub fn parse(path: &Path, text: &str) -> Result<Self, ConfigError> {
        let parse_err = |source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        };
        let mut table: toml::Table = toml::from_str(text).map_err(parse_err)?;
        let section = |table: &mut toml::Table, name: &str| {
            table
                .remove(name)
                .unwrap_or_else(|| toml::Value::Table(toml::Table::new()))
        };
        let game: GameSection = section(&mut table, "game").try_into().map_err(parse_err)?;
        let player: PlayerSection = section(&mut table, "player")
            .try_into()
            .map_err(parse_err)?;

        let mut overrides = toml::Table::new();
        let mut ignored = Vec::new();
        for (name, value) in table {
            if OWN_SECTIONS.contains(&name.as_str()) {
                continue;
            }
            let allowed = OVERRIDABLE.iter().find(|(s, _)| *s == name);
            let (Some((_, keys)), toml::Value::Table(values)) = (allowed, value) else {
                ignored.push(name);
                continue;
            };
            let mut kept = toml::Table::new();
            for (key, value) in values {
                if keys.contains(&key.as_str()) {
                    kept.insert(key, value);
                } else {
                    ignored.push(format!("{name}.{key}"));
                }
            }
            if !kept.is_empty() {
                overrides.insert(name, toml::Value::Table(kept));
            }
        }

        ignored.sort();
        let profile = Self {
            id: path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path: path.to_owned(),
            name: game.name.filter(|n| !n.trim().is_empty()),
            exe: match game.exe {
                Exe::One(exe) => vec![exe],
                Exe::Many(exe) => exe,
            }
            .into_iter()
            .map(|e| e.trim().to_owned())
            .filter(|e| !e.is_empty())
            .collect(),
            steam_appid: game.steam_appid.filter(|&id| id != 0),
            player_names: (player.names.into_iter())
                .map(|n| n.trim().to_owned())
                .filter(|n| !n.is_empty())
                .collect(),
            overrides,
            ignored,
        };
        // A value of the wrong type is an error now, not when the game starts.
        profile.try_apply(&Config::default())?;
        Ok(profile)
    }

    /// The game's name, or the profile's id if the file doesn't give one.
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    /// The settings it sets, as dotted names with their values.
    pub fn overrides(&self) -> Vec<(String, &toml::Value)> {
        let mut out = Vec::new();
        for (section, values) in &self.overrides {
            if let toml::Value::Table(values) = values {
                for (key, value) in values {
                    out.push((format!("{section}.{key}"), value));
                }
            }
        }
        out
    }

    /// Whether it sets dotted `"section.key"`.
    pub fn sets(&self, dotted: &str) -> bool {
        dotted.split_once('.').is_some_and(|(section, key)| {
            (self.overrides.get(section))
                .and_then(toml::Value::as_table)
                .is_some_and(|t| t.contains_key(key))
        })
    }

    /// How well it fits a game, higher is better: 3 for the same Steam app
    /// id and executable, 2 for the Steam app id alone, 1 for the executable
    /// alone. `None` when it doesn't fit, including the same executable name
    /// with a different Steam app id (two games can both ship `game.exe`).
    pub fn fit(&self, exe_name: &str, steam_appid: Option<u32>) -> Option<u8> {
        let exe =
            (!exe_name.is_empty()) && self.exe.iter().any(|e| e.eq_ignore_ascii_case(exe_name));
        let app = match (self.steam_appid, steam_appid) {
            (Some(ours), Some(theirs)) if ours != theirs => return None,
            (Some(_), Some(_)) => true,
            _ => false,
        };
        match (app, exe) {
            (true, true) => Some(3),
            (true, false) => Some(2),
            (false, true) => Some(1),
            (false, false) => None,
        }
    }

    /// `global` with this profile's settings in place of its own, clamped
    /// like any config.
    pub fn apply(&self, global: &Config) -> Config {
        // Values were checked when the profile was parsed.
        self.try_apply(global).unwrap_or_else(|_| global.clone())
    }

    fn try_apply(&self, global: &Config) -> Result<Config, ConfigError> {
        let mut table = toml::Table::try_from(global)?;
        for (section, values) in &self.overrides {
            let Some(values) = values.as_table() else {
                continue;
            };
            let target = table
                .entry(section.clone())
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            if let Some(target) = target.as_table_mut() {
                for (key, value) in values {
                    target.insert(key.clone(), value.clone());
                }
            }
        }
        let mut config: Config = table.try_into().map_err(|source| ConfigError::Parse {
            path: self.path.clone(),
            source,
        })?;
        config.normalize();
        Ok(config)
    }

    /// Writes `keys` (dotted, [`OVERRIDABLE`] only) into the profile's file
    /// with the values `settings` has, keeping the rest of the file as it
    /// is. A key `settings` leaves unset that `global` sets is written as
    /// "unset" (0 or ""), so the profile still undoes it; otherwise it is
    /// taken out. Remakes the file from the template if it was deleted.
    /// Reload the profile afterwards to see the change.
    pub fn save_settings(
        &self,
        settings: &Config,
        global: &Config,
        keys: &[&str],
    ) -> Result<(), ConfigError> {
        let globals = global.key_values(keys)?;
        let edits = (settings.key_values(keys)?.into_iter())
            .zip(globals)
            .map(|((key, value), (_, global))| {
                let value = match (value, global) {
                    (None, Some(_)) => unset_value(key),
                    (value, _) => value,
                };
                (key, value)
            })
            .collect::<Vec<_>>();
        self.edit(&edits)
    }

    /// Takes `keys` out of the profile's file, so they follow `config.toml`
    /// again.
    pub fn clear_settings(&self, keys: &[&str]) -> Result<(), ConfigError> {
        let edits = keys.iter().map(|&k| (k, None)).collect::<Vec<_>>();
        self.edit(&edits)
    }

    fn edit(&self, edits: &[(&str, Option<toml::Value>)]) -> Result<(), ConfigError> {
        if let Some((key, _)) = edits.iter().find(|(k, _)| !overridable(k)) {
            return Err(ConfigError::InvalidKey((*key).to_owned()));
        }
        if !self.path.exists() {
            let exe = self.exe.first().map_or("", String::as_str);
            let text = template(self.display_name(), exe, self.steam_appid);
            config::write_atomic(&self.path, &text)?;
        }
        config::edit_keys(&self.path, edits)
    }
}

/// The profiles in a folder, and the files that couldn't be read.
#[derive(Debug, Default)]
pub struct Profiles {
    /// In file name order.
    pub profiles: Vec<Profile>,
    pub problems: Vec<ConfigError>,
}

impl Profiles {
    /// Reads every `*.toml` in `dir`. A missing folder has no profiles.
    pub fn load(dir: &Path) -> Self {
        let mut found = Self::default();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return found;
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("toml"))
                    && p.is_file()
            })
            .collect();
        paths.sort();
        for path in paths {
            match Profile::load(&path) {
                Ok(profile) => found.profiles.push(profile),
                Err(e) => found.problems.push(e),
            }
        }
        found
    }

    /// The profile that fits a game best (see [`Profile::fit`]); the first
    /// by file name among equals.
    pub fn find(&self, exe_name: &str, steam_appid: Option<u32>) -> Option<&Profile> {
        let mut best: Option<(u8, &Profile)> = None;
        for profile in &self.profiles {
            if let Some(fit) = profile.fit(exe_name, steam_appid)
                && best.is_none_or(|(b, _)| fit > b)
            {
                best = Some((fit, profile));
            }
        }
        best.map(|(_, p)| p)
    }

    /// A profile by id (file name) or executable name, ignoring case.
    pub fn get(&self, name: &str) -> Option<&Profile> {
        let name = name.strip_suffix(".toml").unwrap_or(name);
        (self.profiles.iter())
            .find(|p| p.id.eq_ignore_ascii_case(name))
            .or_else(|| self.find(name, None))
    }
}

/// Makes a profile with no settings of its own for a game rec records for
/// the first time: `<dir>\<exe name>.toml`, or `<exe name>-<app id>.toml`
/// (then `-2`, `-3`…) if another game already has that name.
pub fn create(
    dir: &Path,
    game: &str,
    exe_name: &str,
    steam_appid: Option<u32>,
) -> Result<Profile, ConfigError> {
    let write_err = |path: &Path, source| ConfigError::Write {
        path: path.to_owned(),
        source,
    };
    std::fs::create_dir_all(dir).map_err(|e| write_err(dir, e))?;
    let base = file_id(exe_name);
    let candidates = std::iter::once(base.clone())
        .chain(steam_appid.map(|app| format!("{base}-{app}")))
        .chain((2..100).map(|n| format!("{base}-{n}")));
    let text = template(game, exe_name, steam_appid);
    for id in candidates {
        let path = dir.join(format!("{id}.toml"));
        // `create_new`: never overwrites a profile, even one made a moment
        // ago by another rec.
        let file = OpenOptions::new().write(true).create_new(true).open(&path);
        match file {
            Ok(mut file) => {
                file.write_all(text.as_bytes())
                    .map_err(|e| write_err(&path, e))?;
                drop(file);
                return Profile::load(&path);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(write_err(&path, e)),
        }
    }
    Err(write_err(
        dir,
        io::Error::new(io::ErrorKind::AlreadyExists, "no free profile name"),
    ))
}

/// A file name for a profile from an executable's: `TF_Win64.exe` ->
/// `tf_win64`.
fn file_id(exe_name: &str) -> String {
    let stem = Path::new(exe_name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let id: String = stem
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if id.trim_matches('_').is_empty() {
        "game".into()
    } else {
        id
    }
}

/// The text of a new profile: what it is for, and every section with
/// commented-out examples, so it sets nothing until the user (or the quick
/// panel) does.
pub fn template(game: &str, exe_name: &str, steam_appid: Option<u32>) -> String {
    let quote = |s: &str| toml::Value::String(s.to_owned()).to_string();
    let exe = if exe_name.is_empty() {
        "[]".to_owned()
    } else {
        format!("[{}]", quote(exe_name))
    };
    let steam = match steam_appid {
        Some(app) => format!("steam_appid = {app}"),
        None => "# steam_appid = 440".to_owned(),
    };
    format!(
        "\
# rec settings for {game_line}, made when rec first recorded it.
#
# While rec records this game, a setting here replaces the same one in
# config.toml. Anything not set here (or commented out) follows config.toml.
# Only [replay], [video] and [audio] settings can be set per game. The quick
# panel's \"Settings for\" row writes its settings here, or takes them out.
# Changes apply when rec next starts recording the game, or after `rec reload`.

[game]
name = {name}
# Executables this profile is for. A Steam app id matches too.
exe = {exe}
{steam}

[player]
# Your names in this game, written into each clip's .rec.json for AI tools,
# for example names = [\"RNDAOM\"].
names = []

[replay]
# seconds = 60
# post_roll_seconds = 0

[video]
# quality = \"balanced\"    # performance, balanced or quality
# bitrate_kbps = 0        # 0: from the quality preset
# fps = 60
# codec = \"auto\"          # auto, h264, hevc or av1
# hdr = \"auto\"            # auto, tonemap or off

[audio]
# game = true
# voice_chat = true
# microphone = false
# other_apps = false
",
        game_line = game.replace(['\r', '\n'], " "),
        name = quote(game),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Codec, Quality};

    fn parse(text: &str) -> Profile {
        Profile::parse(Path::new(r"C:\profiles\tf2.toml"), text).unwrap()
    }

    #[test]
    fn template_parses_to_a_profile_that_sets_nothing() {
        let text = template("Team Fortress 2", "tf_win64.exe", Some(440));
        let p = parse(&text);
        assert_eq!(p.id, "tf2");
        assert_eq!(p.name.as_deref(), Some("Team Fortress 2"));
        assert_eq!(p.exe, ["tf_win64.exe"]);
        assert_eq!(p.steam_appid, Some(440));
        assert!(p.player_names.is_empty());
        assert!(p.overrides().is_empty());
        assert!(p.ignored.is_empty());
        let global = Config::default();
        assert_eq!(p.apply(&global), global);

        // Odd names are quoted, and an unknown app id is left commented out.
        let p = parse(&template("Say \"Hi\"\n2", "", None));
        assert_eq!(p.name.as_deref(), Some("Say \"Hi\"\n2"));
        assert!(p.exe.is_empty());
        assert_eq!(p.steam_appid, None);
    }

    #[test]
    fn missing_settings_follow_the_global_config() {
        let mut global = Config::default();
        global.replay.seconds = 120;
        global.replay.post_roll_seconds = 5;
        global.video.codec = Codec::Av1;
        global.audio.microphone = true;
        global.output.quota_gb = Some(50);
        let p = parse(
            "[game]\nexe = \"tf_win64.exe\"\n[replay]\nseconds = 30\n\
             [video]\nquality = \"quality\"\n[audio]\nmicrophone = false\n",
        );
        let c = p.apply(&global);
        assert_eq!(c.replay.seconds, 30);
        assert_eq!(c.video.quality, Quality::Quality);
        assert!(!c.audio.microphone);
        // Not in the profile: from the global config.
        assert_eq!(c.replay.post_roll_seconds, 5);
        assert_eq!(c.video.codec, Codec::Av1);
        assert_eq!(c.output.quota_gb, Some(50));
        assert!(p.sets("replay.seconds"));
        assert!(!p.sets("replay.post_roll_seconds"));
        assert!(!p.sets("seconds"));
    }

    #[test]
    fn values_are_clamped_and_zero_unsets() {
        let mut global = Config::default();
        global.video.bitrate_kbps = Some(40_000);
        global.audio.microphone_device = Some("{mic}".into());
        let p = parse(
            "[replay]\nseconds = 1\n[video]\nbitrate_kbps = 0\n[audio]\nmicrophone_device = \"\"\n",
        );
        let c = p.apply(&global);
        assert_eq!(c.replay.seconds, config::MIN_REPLAY_SECONDS);
        assert_eq!(c.video.bitrate_kbps, None);
        assert_eq!(c.audio.microphone_device, None);
    }

    #[test]
    fn global_only_settings_are_ignored_and_reported() {
        let p = parse(
            "future = 1\n[output]\nquota_gb = 1\n[hotkeys]\nsave_clip = \"Alt+F1\"\n\
             [replay]\nseconds = 30\nstorage = \"disk\"\n[video]\ntarget = \"display\"\n\
             [sources]\ntf2_console = true\n[hud]\nkillfeed = [1, 2, 3, 4]\n",
        );
        assert_eq!(
            p.ignored,
            [
                "future",
                "hotkeys",
                "output",
                "replay.storage",
                "video.target"
            ]
        );
        let global = Config::default();
        let c = p.apply(&global);
        assert_eq!(c.replay.seconds, 30);
        assert_eq!(c.output, global.output);
        assert_eq!(c.hotkeys, global.hotkeys);
        assert_eq!(c.video.target, global.video.target);
        assert_eq!(p.overrides().len(), 1);
    }

    #[test]
    fn bad_values_are_errors() {
        let path = Path::new("x.toml");
        for text in [
            "[replay]\nseconds = \"long\"\n",
            "[video]\ncodec = \"vp9\"\n",
            "[game]\nexe = 3\n",
            "[game]\nsteam_appid = -1\n",
            "[player]\nnames = \"me\"\n",
            "[replay\n",
        ] {
            assert!(Profile::parse(path, text).is_err(), "{text}");
        }
    }

    #[test]
    fn player_names_and_exe_lists() {
        let p = parse(
            "[game]\nexe = [\"Game.exe\", \" game_dx12.exe \", \"\"]\nsteam_appid = 0\n\
             [player]\nnames = [\"RNDAOM\", \" \", \"rnd\"]\n",
        );
        assert_eq!(p.exe, ["Game.exe", "game_dx12.exe"]);
        assert_eq!(p.steam_appid, None);
        assert_eq!(p.player_names, ["RNDAOM", "rnd"]);
        assert_eq!(p.display_name(), "tf2");
    }

    #[test]
    fn fits_by_exe_and_steam_app_id() {
        let p = parse("[game]\nexe = [\"tf_win64.exe\"]\nsteam_appid = 440\n");
        assert_eq!(p.fit("TF_WIN64.EXE", Some(440)), Some(3));
        assert_eq!(p.fit("tf.exe", Some(440)), Some(2));
        assert_eq!(p.fit("tf_win64.exe", None), Some(1));
        // The same exe name in another Steam game is another game.
        assert_eq!(p.fit("tf_win64.exe", Some(730)), None);
        assert_eq!(p.fit("hl2.exe", None), None);
        let no_app = parse("[game]\nexe = [\"game.exe\"]\n");
        assert_eq!(no_app.fit("game.exe", Some(440)), Some(1));
        assert_eq!(no_app.fit("", None), None);
    }

    /// Temp folder, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "rec-profiles-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn finds_the_best_profile_in_a_folder() {
        let dir = TempDir::new();
        let write = |name: &str, text: &str| std::fs::write(dir.0.join(name), text).unwrap();
        write("a_exe.toml", "[game]\nexe = \"game.exe\"\n");
        write(
            "b_app.toml",
            "[game]\nexe = \"game.exe\"\nsteam_appid = 440\n",
        );
        write(
            "c_other.toml",
            "[game]\nexe = \"game.exe\"\nsteam_appid = 730\n",
        );
        write("broken.toml", "[game\n");
        write("notes.txt", "not a profile");
        let found = Profiles::load(&dir.0);
        assert_eq!(found.profiles.len(), 3);
        assert_eq!(found.problems.len(), 1);
        assert_eq!(found.find("game.exe", Some(440)).unwrap().id, "b_app");
        assert_eq!(found.find("game.exe", Some(730)).unwrap().id, "c_other");
        // No app id known: the exe fits all three, the first file wins.
        assert_eq!(found.find("game.exe", None).unwrap().id, "a_exe");
        assert_eq!(found.find("GAME.EXE", Some(570)).unwrap().id, "a_exe");
        assert!(found.find("other.exe", None).is_none());
        assert_eq!(found.get("B_APP.toml").unwrap().id, "b_app");
        assert_eq!(found.get("game.exe").unwrap().id, "a_exe");
        assert!(Profiles::load(&dir.0.join("missing")).profiles.is_empty());
    }

    #[test]
    fn creates_a_profile_once_per_game() {
        let dir = TempDir::new();
        let profiles = dir.0.join("profiles");
        let p = create(&profiles, "Team Fortress 2", "TF_Win64.exe", Some(440)).unwrap();
        assert_eq!(p.id, "tf_win64");
        assert_eq!(p.path, profiles.join("tf_win64.toml"));
        assert_eq!(p.exe, ["TF_Win64.exe"]);
        assert!(p.overrides().is_empty());
        let found = Profiles::load(&profiles);
        assert_eq!(
            found.find("tf_win64.exe", Some(440)).unwrap().id,
            "tf_win64"
        );

        // Another game with the same exe name gets a file of its own, and an
        // existing file is never overwritten.
        std::fs::write(
            &p.path,
            "# mine\n[game]\nexe = \"tf_win64.exe\"\nsteam_appid = 440\n",
        )
        .unwrap();
        let other = create(&profiles, "Other", "tf_win64.exe", Some(999)).unwrap();
        assert_eq!(other.id, "tf_win64-999");
        let third = create(&profiles, "Third", "tf_win64.exe", None).unwrap();
        assert_eq!(third.id, "tf_win64-2");
        assert!(
            std::fs::read_to_string(&p.path)
                .unwrap()
                .starts_with("# mine")
        );

        assert_eq!(file_id("My Game!.exe"), "my_game_");
        assert_eq!(file_id("!!.exe"), "game");
    }

    #[test]
    fn saving_settings_keeps_the_file_and_layers_over_global() {
        let dir = TempDir::new();
        let p = create(&dir.0, "Team Fortress 2", "tf_win64.exe", Some(440)).unwrap();
        let mut global = Config::default();
        global.video.bitrate_kbps = Some(40_000);
        let mut settings = global.clone();
        settings.replay.seconds = 30;
        settings.video.quality = Quality::Quality;
        settings.video.bitrate_kbps = None;
        settings.audio.microphone = true;
        let keys = [
            "replay.seconds",
            "video.quality",
            "video.bitrate_kbps",
            "audio.microphone",
        ];
        p.save_settings(&settings, &global, &keys).unwrap();

        let text = std::fs::read_to_string(&p.path).unwrap();
        // The template's comments stay; values go under their sections.
        assert!(text.starts_with("# rec settings for Team Fortress 2"));
        assert!(text.contains("[replay]\nseconds = 30\n# seconds = 60\n"));
        assert!(text.contains("[video]\nquality = \"quality\"\nbitrate_kbps = 0\n"));
        assert!(text.contains("[audio]\nmicrophone = true\n"));
        let saved = Profile::load(&p.path).unwrap();
        for key in keys {
            assert!(saved.sets(key), "{key}");
        }
        // Global's bitrate is undone, so the preset applies in this game.
        assert_eq!(saved.apply(&global), {
            let mut want = settings.clone();
            want.video.bitrate_kbps = None;
            want
        });

        // Without a global bitrate there is nothing to undo.
        let plain = Config::default();
        saved
            .save_settings(&plain, &plain, &["video.bitrate_kbps"])
            .unwrap();
        assert!(!Profile::load(&p.path).unwrap().sets("video.bitrate_kbps"));

        saved.clear_settings(&keys).unwrap();
        let cleared = Profile::load(&p.path).unwrap();
        assert!(cleared.overrides().is_empty());
        assert_eq!(cleared.apply(&global), global);
        assert!(
            std::fs::read_to_string(&p.path)
                .unwrap()
                .contains("[replay]\n# seconds = 60\n")
        );

        // Global-only settings can't be written to a profile.
        assert!(saved.clear_settings(&["output.quota_gb"]).is_err());

        // A deleted file is made again from the template.
        std::fs::remove_file(&p.path).unwrap();
        cleared
            .save_settings(&settings, &global, &["replay.seconds"])
            .unwrap();
        let remade = Profile::load(&p.path).unwrap();
        assert_eq!(remade.exe, ["tf_win64.exe"]);
        assert_eq!(remade.steam_appid, Some(440));
        assert!(remade.sets("replay.seconds"));
    }

    #[test]
    fn overridable_keys() {
        assert!(overridable("replay.seconds"));
        assert!(overridable("audio.voice_chat_apps"));
        assert!(!overridable("output.quota_gb"));
        assert!(!overridable("video.target"));
        assert!(!overridable("replay"));
        // Every listed key is a real setting.
        let table = toml::Table::try_from(Config::default()).unwrap();
        for (section, keys) in OVERRIDABLE {
            let section_table = table[*section].as_table().unwrap();
            for key in *keys {
                let optional = unset_value(&format!("{section}.{key}")).is_some();
                assert!(
                    optional || section_table.contains_key(*key),
                    "{section}.{key}"
                );
            }
        }
    }
}
