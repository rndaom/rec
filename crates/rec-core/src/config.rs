//! User configuration, stored as TOML at `%APPDATA%\rec\config.toml`.
//!
//! Every section uses `#[serde(default)]`, so a partial or older file loads
//! with defaults for anything missing, and unknown keys from newer versions are
//! ignored. Values are clamped to supported ranges by [`Config::normalize`].

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::hotkey::{Hotkey, Key, Modifiers};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub replay: ReplayConfig,
    pub video: VideoConfig,
    pub audio: AudioConfig,
    pub output: OutputConfig,
    pub hotkeys: HotkeyConfig,
    pub behavior: BehaviorConfig,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReplayConfig {
    /// Seconds of history saved by the main hotkey.
    pub seconds: u32,
    /// Seconds to keep recording after the hotkey before the clip is closed.
    pub post_roll_seconds: u32,
    /// Hard cap on replay buffer memory. The buffer holds encoded packets, so
    /// 60 s at 50 Mbit/s is about 375 MB.
    pub max_memory_mb: u32,
    pub storage: BufferStorage,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            seconds: 60,
            post_roll_seconds: 0,
            max_memory_mb: 1024,
            storage: BufferStorage::Memory,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BufferStorage {
    Memory,
    /// Segment ring on disk, for long or high-bitrate buffers.
    Disk,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoConfig {
    pub target: CaptureTarget,
    /// Display index for `target = "display"`. `None` follows the display the
    /// foreground game is on, falling back to the primary display.
    pub display: Option<u32>,
    pub fps: u32,
    /// Downscale to this height (e.g. 1080). `None` keeps native resolution.
    pub max_height: Option<u32>,
    pub codec: Codec,
    pub quality: Quality,
    /// Overrides the quality preset's bitrate.
    pub bitrate_kbps: Option<u32>,
    pub encoder: EncoderPreference,
    pub cursor: bool,
    pub hdr: HdrMode,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            target: CaptureTarget::Game,
            display: None,
            fps: 60,
            max_height: None,
            codec: Codec::Auto,
            quality: Quality::Balanced,
            bitrate_kbps: None,
            encoder: EncoderPreference::Auto,
            cursor: true,
            hdr: HdrMode::Auto,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureTarget {
    /// Only the detected game's window: no notifications, other windows or
    /// second monitor in clips.
    Game,
    /// A whole display.
    Display,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    /// HEVC where the GPU supports it, else H.264.
    Auto,
    H264,
    Hevc,
    Av1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    Performance,
    Balanced,
    Quality,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncoderPreference {
    Auto,
    Nvidia,
    Amd,
    Intel,
    /// Media Foundation hardware encoder, vendor-agnostic.
    MediaFoundation,
    /// CPU encoding. Costs game performance; opt-in only.
    Software,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HdrMode {
    /// Keep HDR (10-bit, HDR10 metadata) when the display is in HDR mode.
    Auto,
    /// Tone-map HDR to SDR so clips look right everywhere.
    Tonemap,
    /// Always record SDR.
    Off,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Audio from the game's process tree.
    pub game: bool,
    pub voice_chat: bool,
    /// Executables treated as voice chat, recorded on their own track.
    pub voice_chat_apps: Vec<String>,
    pub microphone: bool,
    /// Endpoint ID; `None` uses the default communications device.
    pub microphone_device: Option<String>,
    /// All other system audio.
    pub other_apps: bool,
    /// Also write a single mixed track first, for players and sites that only
    /// play one track.
    pub mixed_track: bool,
    pub bitrate_kbps: u32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            game: true,
            voice_chat: true,
            voice_chat_apps: vec!["Discord.exe".into()],
            microphone: false,
            microphone_device: None,
            other_apps: false,
            mixed_track: true,
            bitrate_kbps: 192,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputConfig {
    /// `None` saves to `<Videos>\rec`.
    pub directory: Option<PathBuf>,
    pub folder_per_game: bool,
    /// Placeholders: `{game}`, `{date}`, `{time}`, `{seconds}`.
    pub filename: String,
    pub container: Container,
    /// Delete the oldest unstarred clips once the folder exceeds this size.
    pub quota_gb: Option<u32>,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            directory: None,
            folder_per_game: true,
            filename: "{game}_{date}_{time}".into(),
            container: Container::Mp4,
            quota_gb: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Container {
    /// Fragmented MP4: plays everywhere and survives a crash mid-write.
    Mp4,
    Mkv,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeyConfig {
    pub save_clip: Hotkey,
    pub toggle_overlay: Hotkey,
    /// Extra save hotkeys with their own lengths (e.g. 15 s, full buffer).
    pub extra_clips: Vec<ClipHotkey>,
    pub add_marker: Option<Hotkey>,
    pub pause: Option<Hotkey>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClipHotkey {
    pub hotkey: Hotkey,
    pub seconds: u32,
}

const ALT: Modifiers = Modifiers {
    ctrl: false,
    alt: true,
    shift: false,
    win: false,
};

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            save_clip: Hotkey::new(ALT, Key::Function(8)),
            toggle_overlay: Hotkey::new(ALT, Key::Function(7)),
            extra_clips: Vec::new(),
            add_marker: None,
            pause: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BehaviorConfig {
    pub start_with_windows: bool,
    pub record_when: RecordWhen,
    pub save_sound: bool,
    pub save_toast: bool,
    /// Stop buffering while the PC runs on battery.
    pub pause_on_battery: bool,
    /// Buffering pauses while any of these is in the foreground.
    pub private_apps: Vec<String>,
    /// Never treat these as games.
    pub ignored_apps: Vec<String>,
    /// Always treat these as games, even windowed.
    pub game_apps: Vec<String>,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            start_with_windows: true,
            record_when: RecordWhen::GameDetected,
            save_sound: true,
            save_toast: true,
            pause_on_battery: false,
            private_apps: vec![
                "1Password.exe".into(),
                "Bitwarden.exe".into(),
                "KeePass.exe".into(),
                "KeePassXC.exe".into(),
            ],
            ignored_apps: Vec::new(),
            game_apps: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordWhen {
    /// Buffer only while a game is running; near-zero cost otherwise.
    GameDetected,
    Always,
    /// Only when started from the tray, overlay, hotkey or CLI.
    Manual,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("could not write {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
    #[error("{path} is not valid: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("could not serialize config: {0}")]
    Serialize(#[from] toml::ser::Error),
}

pub const MIN_REPLAY_SECONDS: u32 = 5;
pub const MAX_REPLAY_SECONDS: u32 = 20 * 60;

impl Config {
    /// Loads the config, returning defaults if the file does not exist.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        Self::from_toml(&text).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        })
    }

    pub fn from_toml(text: &str) -> Result<Self, toml::de::Error> {
        let mut config: Self = toml::from_str(text)?;
        config.normalize();
        Ok(config)
    }

    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// Writes atomically: a temp file next to the target, then a rename.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let text = self.to_toml()?;
        let write_err = |source| ConfigError::Write {
            path: path.to_owned(),
            source,
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(write_err)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(write_err)?;
        std::fs::rename(&tmp, path).map_err(write_err)
    }

    /// Clamps values to supported ranges.
    pub fn normalize(&mut self) {
        let r = &mut self.replay;
        r.seconds = r.seconds.clamp(MIN_REPLAY_SECONDS, MAX_REPLAY_SECONDS);
        r.post_roll_seconds = r.post_roll_seconds.min(60);
        r.max_memory_mb = r.max_memory_mb.clamp(64, 16 * 1024);

        let v = &mut self.video;
        v.fps = v.fps.clamp(10, 240);
        v.max_height = v.max_height.map(|h| h.clamp(360, 4320));
        v.bitrate_kbps = v.bitrate_kbps.map(|b| b.clamp(1_000, 200_000));

        self.audio.bitrate_kbps = self.audio.bitrate_kbps.clamp(64, 512);

        for clip in &mut self.hotkeys.extra_clips {
            clip.seconds = clip.seconds.clamp(MIN_REPLAY_SECONDS, MAX_REPLAY_SECONDS);
        }
        if self.output.filename.trim().is_empty() {
            self.output.filename = OutputConfig::default().filename;
        }
    }
}

/// `%APPDATA%\rec\config.toml`.
pub fn default_config_path() -> PathBuf {
    let base = std::env::var_os("APPDATA").map_or_else(|| PathBuf::from("."), PathBuf::from);
    base.join("rec").join("config.toml")
}

/// Best-effort `%USERPROFILE%\Videos\rec`. `recd` resolves the real Videos
/// known folder (which users can relocate) and only falls back to this.
pub fn fallback_output_dir() -> PathBuf {
    let base = std::env::var_os("USERPROFILE").map_or_else(|| PathBuf::from("."), PathBuf::from);
    base.join("Videos").join("rec")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_round_trips_through_toml() {
        let config = Config::default();
        let text = config.to_toml().unwrap();
        assert_eq!(Config::from_toml(&text).unwrap(), config);
        assert!(text.contains("save_clip = \"Alt+F8\""));
    }

    #[test]
    fn empty_and_partial_files_use_defaults() {
        assert_eq!(Config::from_toml("").unwrap(), Config::default());

        let config =
            Config::from_toml("[replay]\nseconds = 30\n[video]\ncodec = \"av1\"\n").unwrap();
        assert_eq!(config.replay.seconds, 30);
        assert_eq!(
            config.replay.max_memory_mb,
            ReplayConfig::default().max_memory_mb
        );
        assert_eq!(config.video.codec, Codec::Av1);
        assert_eq!(config.video.fps, 60);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let config = Config::from_toml("future_option = 1\n[replay]\nnew_thing = true\n").unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn normalize_clamps_out_of_range_values() {
        let config = Config::from_toml(
            "[replay]\nseconds = 1\npost_roll_seconds = 900\n[video]\nfps = 1000\n[audio]\nbitrate_kbps = 1\n",
        )
        .unwrap();
        assert_eq!(config.replay.seconds, MIN_REPLAY_SECONDS);
        assert_eq!(config.replay.post_roll_seconds, 60);
        assert_eq!(config.video.fps, 240);
        assert_eq!(config.audio.bitrate_kbps, 64);
    }

    #[test]
    fn invalid_hotkey_is_a_parse_error() {
        assert!(Config::from_toml("[hotkeys]\nsave_clip = \"S\"\n").is_err());
    }

    #[test]
    fn extra_clip_hotkeys_parse() {
        let config =
            Config::from_toml("[[hotkeys.extra_clips]]\nhotkey = \"Alt+F6\"\nseconds = 15\n")
                .unwrap();
        assert_eq!(config.hotkeys.extra_clips.len(), 1);
        assert_eq!(config.hotkeys.extra_clips[0].hotkey.to_string(), "Alt+F6");
    }

    #[test]
    fn save_then_load() {
        let dir = std::env::temp_dir().join(format!("rec-config-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        let mut config = Config::default();
        config.replay.seconds = 120;
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_file_loads_defaults() {
        let path = std::env::temp_dir()
            .join("rec-definitely-missing")
            .join("config.toml");
        assert_eq!(Config::load(&path).unwrap(), Config::default());
    }
}
