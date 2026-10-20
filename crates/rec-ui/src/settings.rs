//! The settings editor: `config.toml` and per-game profiles, read and
//! written through rec-core so the files keep the user's comments and
//! layout (`Config::save_keys`, `Profile::save_settings`). recd is asked to
//! reload afterwards; if it isn't running it reads the files when it starts.

use std::path::{Path, PathBuf};

use rec_core::config::{self, Config, ConfigError};
use rec_core::hotkey::Hotkey;
use rec_core::profile::{self, Profile, Profiles};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::recd;

/// One editable setting, as the UI draws it.
#[derive(Debug, Serialize)]
pub struct Field {
    pub key: &'static str,
    pub section: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    #[serde(flatten)]
    pub kind: Kind,
    /// A per-game profile may set it (`rec_core::profile::OVERRIDABLE`).
    pub per_game: bool,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kind {
    Bool,
    /// `optional`: empty means unset (`None`).
    Number {
        min: u32,
        max: u32,
        step: u32,
        unit: &'static str,
        optional: bool,
    },
    Choice {
        options: &'static [(&'static str, &'static str)],
    },
    Text {
        optional: bool,
        placeholder: &'static str,
    },
    Folder,
    Hotkey {
        optional: bool,
    },
    Combo,
    /// Executable names, one per line.
    List,
    /// `hotkeys.extra_clips`.
    ExtraClips,
}

macro_rules! field {
    ($key:literal, $section:literal, $label:literal, $help:literal, $kind:expr) => {
        Field {
            key: $key,
            section: $section,
            label: $label,
            help: $help,
            kind: $kind,
            per_game: false,
        }
    };
}

const fn num(min: u32, max: u32, step: u32, unit: &'static str) -> Kind {
    Kind::Number {
        min,
        max,
        step,
        unit,
        optional: false,
    }
}

const fn opt_num(min: u32, max: u32, step: u32, unit: &'static str) -> Kind {
    Kind::Number {
        min,
        max,
        step,
        unit,
        optional: true,
    }
}

const fn choice(options: &'static [(&'static str, &'static str)]) -> Kind {
    Kind::Choice { options }
}

/// Every setting the UI edits, in display order.
pub fn schema() -> Vec<Field> {
    let mut fields = vec![
        field!(
            "replay.seconds",
            "Replay",
            "Clip length",
            "Seconds of gameplay the save hotkey keeps.",
            num(
                config::MIN_REPLAY_SECONDS,
                config::MAX_REPLAY_SECONDS,
                5,
                "s"
            )
        ),
        field!(
            "replay.post_roll_seconds",
            "Replay",
            "Keep recording after saving",
            "Seconds recorded after the hotkey before the clip is closed.",
            num(0, 60, 1, "s")
        ),
        field!(
            "replay.max_memory_mb",
            "Replay",
            "Buffer memory limit",
            "Hard cap on the replay buffer's memory. 60 s at 50 Mbit/s is about 375 MB.",
            num(64, 16 * 1024, 64, "MB")
        ),
        field!(
            "video.fps",
            "Video",
            "Frame rate",
            "Frames per second recorded.",
            num(10, 240, 1, "fps")
        ),
        field!(
            "video.max_height",
            "Video",
            "Maximum height",
            "Downscale to this height, e.g. 1080. Empty keeps the game's resolution.",
            opt_num(360, 4320, 1, "px")
        ),
        field!(
            "video.quality",
            "Video",
            "Quality",
            "Bitrate preset for the encoder.",
            choice(&[
                ("performance", "Performance"),
                ("balanced", "Balanced"),
                ("quality", "Quality")
            ])
        ),
        field!(
            "video.bitrate_kbps",
            "Video",
            "Bitrate",
            "Overrides the quality preset. Empty uses the preset.",
            opt_num(1_000, 200_000, 500, "kbit/s")
        ),
        field!(
            "video.codec",
            "Video",
            "Codec",
            "Auto picks HEVC where the GPU has it, else H.264. H.264 plays everywhere; AV1 makes the smallest files.",
            choice(&[
                ("auto", "Auto"),
                ("h264", "H.264"),
                ("hevc", "HEVC"),
                ("av1", "AV1")
            ])
        ),
        field!(
            "video.encoder",
            "Video",
            "Encoder",
            "Auto uses the GPU vendor's native encoder where rec has one, else Media Foundation.",
            choice(&[
                ("auto", "Auto"),
                ("amd", "AMD AMF"),
                ("nvidia", "NVIDIA"),
                ("intel", "Intel"),
                ("media_foundation", "Media Foundation")
            ])
        ),
        field!(
            "video.hdr",
            "Video",
            "HDR",
            "Auto keeps HDR (10-bit HEVC or AV1) when the display is in HDR mode; Tone-map makes SDR clips that look right everywhere.",
            choice(&[
                ("auto", "Auto"),
                ("tonemap", "Tone-map to SDR"),
                ("off", "Off")
            ])
        ),
        field!(
            "video.cursor",
            "Video",
            "Record the mouse cursor",
            "",
            Kind::Bool
        ),
        field!(
            "video.target",
            "Video",
            "Capture",
            "Game records only the game's window: no notifications or second monitor in clips.",
            choice(&[("game", "Game window"), ("display", "Whole display")])
        ),
        field!(
            "video.capture",
            "Video",
            "Capture method",
            "Leave on Auto; the others are for troubleshooting.",
            choice(&[
                ("auto", "Auto"),
                ("wgc", "Windows.Graphics.Capture"),
                ("dda", "Desktop Duplication")
            ])
        ),
        field!(
            "video.gpu_priority",
            "Video",
            "GPU priority",
            "Adaptive raises rec's GPU priority only while a GPU-bound game makes frames drop.",
            choice(&[
                ("adaptive", "Adaptive"),
                ("normal", "Normal"),
                ("high", "High")
            ])
        ),
        field!(
            "audio.game",
            "Audio",
            "Game audio",
            "Sound from the game's process tree.",
            Kind::Bool
        ),
        field!(
            "audio.voice_chat",
            "Audio",
            "Voice chat",
            "Voice chat apps on their own track.",
            Kind::Bool
        ),
        field!(
            "audio.voice_chat_apps",
            "Audio",
            "Voice chat apps",
            "Executables recorded as voice chat, one per line.",
            Kind::List
        ),
        field!(
            "audio.microphone",
            "Audio",
            "Microphone",
            "Applies from the next recording when turned on mid-game.",
            Kind::Bool
        ),
        field!(
            "audio.microphone_device",
            "Audio",
            "Microphone device",
            "Endpoint ID. Empty uses the default communications device.",
            Kind::Text {
                optional: true,
                placeholder: "Default device"
            }
        ),
        field!(
            "audio.other_apps",
            "Audio",
            "Other apps",
            "All other system sound.",
            Kind::Bool
        ),
        field!(
            "audio.mixed_track",
            "Audio",
            "Mixed track first",
            "A single mixed track for players and sites that play only one.",
            Kind::Bool
        ),
        field!(
            "audio.bitrate_kbps",
            "Audio",
            "Audio bitrate",
            "",
            num(64, 512, 32, "kbit/s")
        ),
        field!(
            "output.directory",
            "Saving",
            "Clips folder",
            "Empty saves to Videos\\rec.",
            Kind::Folder
        ),
        field!(
            "output.folder_per_game",
            "Saving",
            "A folder per game",
            "",
            Kind::Bool
        ),
        field!(
            "output.filename",
            "Saving",
            "File name",
            "Placeholders: {game}, {date}, {time}, {seconds}.",
            Kind::Text {
                optional: false,
                placeholder: "{game}_{date}_{time}"
            }
        ),
        field!(
            "output.quota_gb",
            "Saving",
            "Storage limit",
            "Keeps rec's clips under this size by deleting the oldest for good. Starred and read-only clips are never deleted. Empty: no limit.",
            opt_num(1, 100_000, 1, "GB")
        ),
        field!(
            "output.sidecar",
            "Saving",
            "Clip details file",
            "Writes <clip>.rec.json next to each clip: game, markers, audio tracks and loudness, for the library and AI tools.",
            Kind::Bool
        ),
        field!(
            "hotkeys.save_clip",
            "Hotkeys",
            "Save clip",
            "",
            Kind::Hotkey { optional: false }
        ),
        field!(
            "hotkeys.toggle_overlay",
            "Hotkeys",
            "Quick panel",
            "",
            Kind::Hotkey { optional: false }
        ),
        field!(
            "hotkeys.extra_clips",
            "Hotkeys",
            "Extra clip lengths",
            "More save hotkeys, each with its own length.",
            Kind::ExtraClips
        ),
        field!(
            "hotkeys.add_marker",
            "Hotkeys",
            "Add marker",
            "Marks a moment; markers show on the clip's timeline.",
            Kind::Hotkey { optional: true }
        ),
        field!(
            "hotkeys.star_last",
            "Hotkeys",
            "Star last clip",
            "Starred clips are never deleted by the storage limit.",
            Kind::Hotkey { optional: true }
        ),
        field!(
            "hotkeys.pause",
            "Hotkeys",
            "Pause / resume",
            "",
            Kind::Hotkey { optional: true }
        ),
        field!(
            "hotkeys.controller_save",
            "Controller",
            "Save clip",
            "Buttons held together, e.g. LS+RS. Off by default: the game sees the buttons too.",
            Kind::Combo
        ),
        field!(
            "hotkeys.controller_add_marker",
            "Controller",
            "Add marker",
            "",
            Kind::Combo
        ),
        field!(
            "hotkeys.controller_star_last",
            "Controller",
            "Star last clip",
            "",
            Kind::Combo
        ),
        field!(
            "hotkeys.controller_hold_ms",
            "Controller",
            "Hold time",
            "How long a combo must be held before it acts.",
            num(
                config::MIN_CONTROLLER_HOLD_MS,
                config::MAX_CONTROLLER_HOLD_MS,
                50,
                "ms"
            )
        ),
        field!(
            "behavior.record_when",
            "Behavior",
            "Record",
            "Game detected costs almost nothing while no game is running.",
            choice(&[
                ("game_detected", "When a game is running"),
                ("always", "Always"),
                ("manual", "Only when started by hand")
            ])
        ),
        field!(
            "behavior.save_sound",
            "Behavior",
            "Sound when a clip is saved",
            "",
            Kind::Bool
        ),
        field!(
            "behavior.save_toast",
            "Behavior",
            "Notification when a clip is saved",
            "",
            Kind::Bool
        ),
        field!(
            "behavior.pause_on_battery",
            "Behavior",
            "Pause on battery",
            "",
            Kind::Bool
        ),
        field!(
            "behavior.start_with_windows",
            "Behavior",
            "Start with Windows",
            "",
            Kind::Bool
        ),
        field!(
            "behavior.game_apps",
            "Games",
            "Always games",
            "Executables always recorded, even windowed.",
            Kind::List
        ),
        field!(
            "behavior.ignored_apps",
            "Games",
            "Never games",
            "Executables never treated as games.",
            Kind::List
        ),
        field!(
            "behavior.private_apps",
            "Games",
            "Private apps",
            "Recording pauses while one of these is in front.",
            Kind::List
        ),
    ];
    for f in &mut fields {
        f.per_game = profile::overridable(f.key);
    }
    fields
}

#[derive(Debug, Serialize)]
pub struct ProfileInfo {
    pub id: String,
    pub name: String,
    pub exe: Vec<String>,
    pub path: PathBuf,
    pub count: usize,
}

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub config_path: PathBuf,
    pub profiles_dir: PathBuf,
    /// The global settings, as `config.toml` gives them (defaults filled in).
    pub values: Value,
    pub profiles: Vec<ProfileInfo>,
    /// The chosen profile: what it sets, and what the game gets in the end.
    pub profile: Option<ProfileView>,
    /// Files that can't be read; editing is refused until they are fixed.
    pub problems: Vec<String>,
    /// Hotkeys other software is known to take, and duplicates.
    pub warnings: Vec<String>,
    pub schema: Vec<Field>,
}

#[derive(Debug, Serialize)]
pub struct ProfileView {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub overrides: serde_json::Map<String, Value>,
    pub effective: Value,
    pub ignored: Vec<String>,
}

/// Where the files are. `REC_UI_CONFIG` points at another config (tests,
/// trying settings out); profiles live next to it either way.
pub fn config_path() -> PathBuf {
    std::env::var_os("REC_UI_CONFIG").map_or_else(config::default_config_path, PathBuf::from)
}

fn profiles_dir(config: &Path) -> PathBuf {
    config
        .parent()
        .map_or_else(profile::default_dir, |d| d.join("profiles"))
}

fn load_global(path: &Path) -> Result<Config, String> {
    Config::load(path).map_err(|e| e.to_string())
}

pub fn snapshot(profile_id: Option<&str>) -> Snapshot {
    let config_path = config_path();
    let dir = profiles_dir(&config_path);
    let mut problems = Vec::new();
    let global = load_global(&config_path).unwrap_or_else(|e| {
        problems.push(e);
        Config::default()
    });
    let loaded = Profiles::load(&dir);
    problems.extend(loaded.problems.iter().map(ToString::to_string));
    let profiles = loaded
        .profiles
        .iter()
        .map(|p| ProfileInfo {
            id: p.id.clone(),
            name: p.display_name().to_owned(),
            exe: p.exe.clone(),
            path: p.path.clone(),
            count: p.overrides().len(),
        })
        .collect();
    let profile = profile_id
        .and_then(|id| loaded.profiles.iter().find(|p| p.id == id))
        .map(|p| ProfileView {
            id: p.id.clone(),
            name: p.display_name().to_owned(),
            path: p.path.clone(),
            overrides: p
                .overrides()
                .into_iter()
                .map(|(k, v)| (k, json!(v)))
                .collect(),
            effective: to_json(&p.apply(&global)),
            ignored: p.ignored.clone(),
        });
    Snapshot {
        config_path,
        profiles_dir: dir,
        values: to_json(&global),
        profiles,
        profile,
        problems,
        warnings: warnings(&global),
        schema: schema(),
    }
}

fn to_json(config: &Config) -> Value {
    serde_json::to_value(config).unwrap_or(Value::Null)
}

fn warnings(config: &Config) -> Vec<String> {
    let h = &config.hotkeys;
    let mut keys: Vec<(&str, &Hotkey)> = vec![
        ("Save clip", &h.save_clip),
        ("Quick panel", &h.toggle_overlay),
    ];
    keys.extend(h.add_marker.as_ref().map(|k| ("Add marker", k)));
    keys.extend(h.star_last.as_ref().map(|k| ("Star last clip", k)));
    keys.extend(h.pause.as_ref().map(|k| ("Pause", k)));
    keys.extend(h.extra_clips.iter().map(|c| ("Extra clip", &c.hotkey)));
    let mut out = Vec::new();
    for (i, (name, key)) in keys.iter().enumerate() {
        if let Some(who) = key.known_conflict() {
            out.push(format!("{name} ({key}) is also {who}."));
        }
        if let Some((other, _)) = keys[..i].iter().find(|(_, k)| k == key) {
            out.push(format!("{name} and {other} both use {key}."));
        }
    }
    out
}

/// A setting to change; `value: null` puts it back to the default (global)
/// or back to following `config.toml` (profile).
#[derive(Debug, Deserialize)]
pub struct Change {
    pub key: String,
    pub value: Value,
}

#[derive(Debug, Serialize)]
pub struct Saved {
    /// recd's answer to the reload, if it is running.
    pub reload: Option<String>,
}

/// Applies `changes` to the global settings or a profile, then asks recd to
/// reload. Nothing is written if any change is invalid.
pub fn apply(profile_id: Option<&str>, changes: &[Change]) -> Result<Saved, String> {
    let config_path = config_path();
    let global = load_global(&config_path)?;
    let mut set = Vec::new();
    let mut cleared = Vec::new();
    for c in changes {
        if !schema().iter().any(|f| f.key == c.key) {
            return Err(format!("`{}` is not a setting the app edits", c.key));
        }
        if c.value.is_null() && profile_id.is_some() {
            cleared.push(c.key.as_str());
        } else {
            set.push(c);
        }
    }
    match profile_id {
        None => {
            let updated = with_values(&global, &set)?;
            let keys: Vec<&str> = set.iter().map(|c| c.key.as_str()).collect();
            updated
                .save_keys(&config_path, &keys)
                .map_err(|e| e.to_string())?;
        }
        Some(id) => {
            let dir = profiles_dir(&config_path);
            let p = find_profile(&dir, id)?;
            if let Some(c) = changes.iter().find(|c| !profile::overridable(&c.key)) {
                return Err(format!("`{}` can't be set per game", c.key));
            }
            if !set.is_empty() {
                let settings = with_values(&p.apply(&global), &set)?;
                let keys: Vec<&str> = set.iter().map(|c| c.key.as_str()).collect();
                p.save_settings(&settings, &global, &keys)
                    .map_err(|e| e.to_string())?;
            }
            if !cleared.is_empty() {
                p.clear_settings(&cleared).map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(Saved {
        reload: recd::reload(),
    })
}

fn find_profile(dir: &Path, id: &str) -> Result<Profile, String> {
    Profiles::load(dir)
        .profiles
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| format!("no profile `{id}`"))
}

/// `base` with dotted settings replaced, checked and clamped like a file
/// that says so. `null` removes the key, so the section's default applies.
fn with_values(base: &Config, changes: &[&Change]) -> Result<Config, String> {
    let mut table = toml::Table::try_from(base).map_err(|e| e.to_string())?;
    for c in changes {
        let (section, key) = c
            .key
            .split_once('.')
            .ok_or_else(|| ConfigError::InvalidKey(c.key.clone()).to_string())?;
        let target = table
            .entry(section)
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .ok_or_else(|| format!("`{section}` is not a section"))?;
        if c.value.is_null() {
            target.remove(key);
        } else {
            let value = toml::Value::try_from(&c.value).map_err(|e| format!("{}: {e}", c.key))?;
            target.insert(key.to_owned(), value);
        }
    }
    let mut config: Config = table
        .try_into()
        .map_err(|e: toml::de::Error| e.message().to_owned())?;
    config.normalize();
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(key: &str, value: Value) -> Change {
        Change {
            key: key.into(),
            value,
        }
    }

    #[test]
    fn every_field_is_a_config_key() {
        let values = to_json(&Config::default());
        for f in schema() {
            let (section, key) = f.key.split_once('.').unwrap();
            let known = values[section].as_object().unwrap();
            // Unset optional settings are absent when serialized.
            let optional = matches!(
                f.kind,
                Kind::Number { optional: true, .. }
                    | Kind::Text { optional: true, .. }
                    | Kind::Hotkey { optional: true }
                    | Kind::Combo
                    | Kind::Folder
            );
            assert!(optional || known.contains_key(key), "{}", f.key);
        }
    }

    #[test]
    fn values_are_checked_and_clamped() {
        let base = Config::default();
        let c = change("video.fps", json!(1000));
        let config = with_values(&base, &[&c]).unwrap();
        assert_eq!(config.video.fps, 240);
        let bad = change("hotkeys.save_clip", json!("S"));
        assert!(with_values(&base, &[&bad]).is_err());
        let wrong_type = change("video.codec", json!("mpeg2"));
        assert!(with_values(&base, &[&wrong_type]).is_err());
    }

    #[test]
    fn null_restores_the_default() {
        let mut base = Config::default();
        base.video.fps = 30;
        base.video.max_height = Some(720);
        let fps = change("video.fps", Value::Null);
        let height = change("video.max_height", Value::Null);
        let config = with_values(&base, &[&fps, &height]).unwrap();
        assert_eq!(config.video.fps, 60);
        assert_eq!(config.video.max_height, None);
    }

    #[test]
    fn saving_keeps_comments() {
        let dir = std::env::temp_dir().join(format!("rec-ui-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "# mine\n[video]\nfps = 60 # smooth\n").unwrap();
        let base = Config::load(&path).unwrap();
        let c = change("video.fps", json!(120));
        let config = with_values(&base, &[&c]).unwrap();
        config.save_keys(&path, &["video.fps"]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# mine"), "{text}");
        assert!(text.contains("fps = 120"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn duplicate_hotkeys_warn() {
        let mut config = Config::default();
        config.hotkeys.pause = Some(config.hotkeys.save_clip);
        assert!(warnings(&config).iter().any(|w| w.contains("both use")));
    }
}
