//! User configuration, stored as TOML at `%APPDATA%\rec\config.toml`.
//!
//! Every section uses `#[serde(default)]`, so a partial or older file loads
//! with defaults for anything missing, and unknown keys from newer versions are
//! ignored. Values are clamped to supported ranges by [`Config::normalize`].

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::controller::Combo;
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
    /// How the picture is captured. Leave on `auto`; the others are for
    /// troubleshooting.
    pub capture: CaptureApi,
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
    /// GPU scheduling priority of rec's capture work against the game's.
    pub gpu_priority: GpuPriority,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            target: CaptureTarget::Game,
            display: None,
            capture: CaptureApi::Auto,
            fps: 60,
            max_height: None,
            codec: Codec::Auto,
            quality: Quality::Balanced,
            bitrate_kbps: None,
            encoder: EncoderPreference::Auto,
            cursor: true,
            hdr: HdrMode::Auto,
            gpu_priority: GpuPriority::Adaptive,
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
pub enum CaptureApi {
    /// Windows.Graphics.Capture where it can hide its yellow border
    /// (Windows 11), else Desktop Duplication.
    Auto,
    /// Windows.Graphics.Capture. On Windows 10 it draws a yellow border
    /// around the game.
    Wgc,
    /// Desktop Duplication of the game's display, cropped to its window:
    /// anything on top of the game is recorded too, and the mouse cursor is
    /// not. Cannot capture a rotated display.
    Dda,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    /// HEVC where the GPU supports it, else H.264.
    Auto,
    /// Plays everywhere; no HDR (an HDR display is tone-mapped to SDR).
    H264,
    Hevc,
    /// Smallest files; needs an AV1 hardware encoder (Radeon RX 7000,
    /// GeForce RTX 40, Intel Arc and newer).
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
    /// The GPU vendor's native encoder where rec has one (AMD AMF), else
    /// Media Foundation.
    Auto,
    /// No native NVENC backend yet: NVIDIA's Media Foundation encoder.
    Nvidia,
    /// AMD AMF, falling back to Media Foundation if it cannot start.
    Amd,
    /// No native oneVPL backend yet: Intel's Media Foundation encoder.
    Intel,
    /// Media Foundation hardware encoder, vendor-agnostic.
    MediaFoundation,
    /// CPU encoding. Costs game performance; opt-in only. Not available
    /// yet: the hardware encoder is used.
    Software,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HdrMode {
    /// Keep HDR (10-bit HEVC or AV1, HDR10 metadata) when the display is in
    /// HDR mode; with H.264, or an encoder without 10-bit, tone-map instead.
    Auto,
    /// Tone-map HDR to SDR so clips look right everywhere.
    Tonemap,
    /// Capture SDR as Windows presents it; HDR highlights clip.
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuPriority {
    /// Normal like any app; raised only while a GPU-saturating game makes
    /// frames drop and raising helps, then lowered again (decision D15).
    Adaptive,
    /// Always normal.
    Normal,
    /// Always high. Costs the game nothing measurable where it does not
    /// help, but is never needed while frames don't drop.
    High,
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
    /// Keep rec's clips in the folder under this many GB (as Explorer counts
    /// them) by deleting the oldest for good. Starred and read-only clips are
    /// never deleted and don't count. `None` or 0: no limit.
    pub quota_gb: Option<u32>,
    /// Write `<clip>.rec.json` next to each clip: game, timing, audio track
    /// roles, markers and loudness, for AI tools and the library.
    pub sidecar: bool,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            directory: None,
            folder_per_game: true,
            filename: "{game}_{date}_{time}".into(),
            container: Container::Mp4,
            quota_gb: None,
            sidecar: true,
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
    /// Stars the clip saved last, so `output.quota_gb` never deletes it.
    pub star_last: Option<Hotkey>,
    pub pause: Option<Hotkey>,
    /// Controller combo that saves a clip, e.g. `"LS+RS"`. Read with XInput,
    /// only while a game is running. Off by default: the game sees the
    /// buttons too, so the right combo depends on the games played.
    pub controller_save: Option<Combo>,
    pub controller_add_marker: Option<Combo>,
    pub controller_star_last: Option<Combo>,
    /// How long a controller combo must be held before it acts, so that
    /// pressing the buttons together in play doesn't trigger it.
    pub controller_hold_ms: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClipHotkey {
    pub hotkey: Hotkey,
    pub seconds: u32,
    /// A controller combo for the same length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller: Option<Combo>,
}

impl HotkeyConfig {
    /// Whether any controller combo is set.
    pub fn any_controller(&self) -> bool {
        self.controller_save.is_some()
            || self.controller_add_marker.is_some()
            || self.controller_star_last.is_some()
            || self.extra_clips.iter().any(|c| c.controller.is_some())
    }
}

pub const MIN_CONTROLLER_HOLD_MS: u32 = 100;
pub const MAX_CONTROLLER_HOLD_MS: u32 = 5000;

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
            star_last: None,
            pause: None,
            controller_save: None,
            controller_add_marker: None,
            controller_star_last: None,
            controller_hold_ms: 500,
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
    #[error("`{0}` is not a `section.key` setting name")]
    InvalidKey(String),
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
        write_atomic(path, &self.to_toml()?)
    }

    /// Writes only `keys` (dotted `"section.key"`, one level deep) from `self`
    /// to the file at `path`, leaving everything else in the file as it is
    /// (comments, order, formatting, keys from newer versions). A key that is
    /// `None` in `self` (so absent when serialized) is removed from the file.
    /// Creates the file with [`Config::save`] if it does not exist. Never
    /// clobbers a file it cannot parse: that returns the parse error.
    ///
    /// The config file is meant to be edited by hand, and a full [`save`]
    /// would throw away the user's comments and any setting `self` does not
    /// know about. So the file's text is patched by [`edit_keys`] instead.
    ///
    /// [`save`]: Config::save
    pub fn save_keys(&self, path: &Path, keys: &[&str]) -> Result<(), ConfigError> {
        let edits = self.key_values(keys)?;
        match edit_keys(path, &edits) {
            Err(ConfigError::Read { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                self.save(path)
            }
            result => result,
        }
    }

    /// The values `self` has for dotted `"section.key"` names, as they would
    /// be written to a file; `None` for a key that is absent when serialized
    /// (an unset optional setting).
    pub fn key_values<'k>(
        &self,
        keys: &[&'k str],
    ) -> Result<Vec<(&'k str, Option<toml::Value>)>, ConfigError> {
        let ours = toml::Table::try_from(self)?;
        keys.iter()
            .map(|&dotted| {
                let (section, key) = split_key(dotted)?;
                let value = ours
                    .get(section)
                    .and_then(toml::Value::as_table)
                    .and_then(|table| table.get(key))
                    .cloned();
                Ok((dotted, value))
            })
            .collect()
    }

    /// Clamps values to supported ranges.
    pub fn normalize(&mut self) {
        let r = &mut self.replay;
        r.seconds = r.seconds.clamp(MIN_REPLAY_SECONDS, MAX_REPLAY_SECONDS);
        r.post_roll_seconds = r.post_roll_seconds.min(60);
        r.max_memory_mb = r.max_memory_mb.clamp(64, 16 * 1024);

        // 0 and "" mean "unset" for optional settings, so a per-game profile
        // (`crate::profile`) can undo one the config file sets.
        let v = &mut self.video;
        v.fps = v.fps.clamp(10, 240);
        v.max_height = (v.max_height.filter(|&h| h != 0)).map(|h| h.clamp(360, 4320));
        v.bitrate_kbps = (v.bitrate_kbps.filter(|&b| b != 0)).map(|b| b.clamp(1_000, 200_000));

        let a = &mut self.audio;
        a.bitrate_kbps = a.bitrate_kbps.clamp(64, 512);
        if a.microphone_device
            .as_deref()
            .is_some_and(|d| d.trim().is_empty())
        {
            a.microphone_device = None;
        }

        for clip in &mut self.hotkeys.extra_clips {
            clip.seconds = clip.seconds.clamp(MIN_REPLAY_SECONDS, MAX_REPLAY_SECONDS);
        }
        let h = &mut self.hotkeys;
        h.controller_hold_ms = h
            .controller_hold_ms
            .clamp(MIN_CONTROLLER_HOLD_MS, MAX_CONTROLLER_HOLD_MS);
        if self.output.quota_gb == Some(0) {
            self.output.quota_gb = None;
        }
        if self.output.filename.trim().is_empty() {
            self.output.filename = OutputConfig::default().filename;
        }
    }
}

/// Sets or removes (`None`) dotted `"section.key"` settings in the TOML file
/// at `path`, leaving everything else in it as it is: comments, order,
/// formatting and keys rec doesn't know. Used for the config file and for
/// per-game profiles (`crate::profile`). The file must exist (a missing one
/// is a [`ConfigError::Read`] with `NotFound`), and a file that doesn't parse
/// is never touched: that returns the parse error.
///
/// The text is patched line by line, then re-parsed and checked to differ
/// from the original in exactly the requested keys; layouts the line editor
/// does not understand (dotted keys, inline tables, multi-line values) fail
/// that check and fall back to editing the parsed table and re-serializing
/// it, which loses comments but still keeps every other setting.
pub fn edit_keys(path: &Path, edits: &[(&str, Option<toml::Value>)]) -> Result<(), ConfigError> {
    let wanted = edits
        .iter()
        .map(|(dotted, value)| {
            let (section, key) = split_key(dotted)?;
            Ok(Wanted {
                section,
                key,
                value: value.as_ref(),
            })
        })
        .collect::<Result<Vec<_>, ConfigError>>()?;
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_owned(),
        source,
    })?;
    let original: toml::Table = toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_owned(),
        source,
    })?;
    let new_text = match patch_text(&text, &original, &wanted) {
        Some(patched) => patched,
        None => rewrite_table(&text, &original, &wanted)?,
    };
    write_atomic(path, &new_text)
}

/// Writes via a temp file next to the target, then renames over it, so a
/// crash never leaves a half-written config.
pub(crate) fn write_atomic(path: &Path, text: &str) -> Result<(), ConfigError> {
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

/// One setting requested from [`Config::save_keys`] with the value `self` has
/// for it (`None`: the key is absent and must be removed from the file).
struct Wanted<'a> {
    section: &'a str,
    key: &'a str,
    value: Option<&'a toml::Value>,
}

/// Splits `"section.key"`. Both parts must be bare TOML keys, which every
/// config field name is, so inserted lines never need quoting.
fn split_key(dotted: &str) -> Result<(&str, &str), ConfigError> {
    let bare = |s: &str| !s.is_empty() && s.bytes().all(is_bare_key_byte);
    match dotted.split_once('.') {
        Some((section, key)) if bare(section) && bare(key) => Ok((section, key)),
        _ => Err(ConfigError::InvalidKey(dotted.to_owned())),
    }
}

fn is_bare_key_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Edits `text` in place and checks the result: it must parse, hold exactly
/// the wanted values, and differ from `original` in nothing else. `None` means
/// the layout is one the line editor cannot handle safely.
fn patch_text(text: &str, original: &toml::Table, wanted: &[Wanted]) -> Option<String> {
    let rendered: Vec<Option<String>> = wanted
        .iter()
        .map(|w| w.value.map(ToString::to_string))
        .collect();
    // Only single-line values can replace a single line.
    if rendered.iter().flatten().any(|v| v.contains(['\n', '\r'])) {
        return None;
    }

    let mut editor = LineEditor::new(text);
    for (w, value) in wanted.iter().zip(&rendered) {
        editor.set(w.section, w.key, value.as_deref());
    }
    let patched = editor.finish();

    let edited: toml::Table = toml::from_str(&patched).ok()?;
    let values_match = wanted.iter().all(|w| lookup(&edited, w) == w.value);
    (values_match && without_wanted(&edited, wanted) == without_wanted(original, wanted))
        .then_some(patched)
}

fn lookup<'a>(table: &'a toml::Table, w: &Wanted) -> Option<&'a toml::Value> {
    table
        .get(w.section)
        .and_then(toml::Value::as_table)
        .and_then(|section| section.get(w.key))
}

/// Copy of `table` without the wanted keys (and sections they leave empty),
/// so two tables can be compared on everything else.
fn without_wanted(table: &toml::Table, wanted: &[Wanted]) -> toml::Table {
    let mut table = table.clone();
    for w in wanted {
        let emptied = match table.get_mut(w.section) {
            Some(toml::Value::Table(section)) => {
                section.remove(w.key);
                section.is_empty()
            }
            _ => false,
        };
        if emptied {
            table.remove(w.section);
        }
    }
    table
}

/// Fallback for layouts [`patch_text`] rejects: apply the wanted keys to the
/// parsed original and re-serialize it. Comments and formatting are lost, but
/// every other setting in the file survives.
fn rewrite_table(
    text: &str,
    original: &toml::Table,
    wanted: &[Wanted],
) -> Result<String, ConfigError> {
    let mut table = original.clone();
    for w in wanted {
        if let Some(value) = w.value {
            let section = table
                .entry(w.section)
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            if !section.is_table() {
                *section = toml::Value::Table(toml::Table::new());
            }
            if let Some(section) = section.as_table_mut() {
                section.insert(w.key.to_owned(), value.clone());
            }
        } else if let Some(section) = table.get_mut(w.section).and_then(toml::Value::as_table_mut) {
            section.remove(w.key);
        }
    }
    let out = toml::to_string_pretty(&table)?;
    Ok(if text.contains("\r\n") {
        out.replace('\n', "\r\n")
    } else {
        out
    })
}

/// Line-based TOML editor that touches only the lines of the keys it is asked
/// about. It understands just enough syntax (table headers, `key = value`
/// lines, trailing comments); [`patch_text`] verifies what it produced.
struct LineEditor {
    lines: Vec<String>,
    eol: &'static str,
    bom: bool,
    final_newline: bool,
}

/// A `[table]` header line.
enum Header {
    /// `[name]` with a single key.
    Table(String),
    /// `[[array]]`, dotted or otherwise unusual names: ends a section, never
    /// matches one.
    Other,
}

impl LineEditor {
    fn new(text: &str) -> Self {
        let (bom, body) = match text.strip_prefix('\u{feff}') {
            Some(rest) => (true, rest),
            None => (false, text),
        };
        Self {
            lines: body.lines().map(str::to_owned).collect(),
            eol: if body.contains("\r\n") { "\r\n" } else { "\n" },
            bom,
            final_newline: body.is_empty() || body.ends_with('\n'),
        }
    }

    fn finish(self) -> String {
        let mut out = String::new();
        if self.bom {
            out.push('\u{feff}');
        }
        out.push_str(&self.lines.join(self.eol));
        if self.final_newline && !self.lines.is_empty() {
            out.push_str(self.eol);
        }
        out
    }

    /// Sets `section.key` to the rendered inline `value`, or removes it.
    fn set(&mut self, section: &str, key: &str, value: Option<&str>) {
        let header = self
            .lines
            .iter()
            .position(|l| matches!(parse_header(l), Some(Header::Table(name)) if name == section));
        let Some(header) = header else {
            if let Some(value) = value {
                if self.lines.last().is_some_and(|l| !l.trim().is_empty()) {
                    self.lines.push(String::new());
                }
                self.lines.push(format!("[{section}]"));
                self.lines.push(format!("{key} = {value}"));
            }
            return;
        };
        let body = header + 1;
        let end = self.lines[body..]
            .iter()
            .position(|l| parse_header(l).is_some())
            .map_or(self.lines.len(), |i| body + i);

        let existing = (body..end).find_map(|i| match parse_key_line(&self.lines[i]) {
            Some((name, value_start)) if name == key => Some((i, value_start)),
            _ => None,
        });
        match (existing, value) {
            (Some((i, value_start)), Some(value)) => {
                self.lines[i] = replace_value(&self.lines[i], value_start, value);
            }
            (Some((i, _)), None) => {
                self.lines.remove(i);
            }
            (None, Some(value)) => {
                // After the last content line, so comments and blank lines
                // that lead into the next section stay with that section.
                let last = (body..end).rev().find(|&i| {
                    let line = self.lines[i].trim();
                    !line.is_empty() && !line.starts_with('#')
                });
                self.lines
                    .insert(last.unwrap_or(header) + 1, format!("{key} = {value}"));
            }
            (None, None) => {}
        }
    }
}

/// Parses `[name]` / `[[name]]` lines (surrounding whitespace and a trailing
/// comment allowed). Lines like `[1, 2],` inside a multi-line array are not
/// headers.
fn parse_header(line: &str) -> Option<Header> {
    let t = line.trim();
    if !t.starts_with('[') {
        return None;
    }
    let (open, close) = if t.starts_with("[[") {
        ("[[", "]]")
    } else {
        ("[", "]")
    };
    let inner_start = open.len();
    let mut quote = None;
    let mut escaped = false;
    let mut inner_end = None;
    for (i, c) in t.char_indices().skip(inner_start) {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if c == '\\' => escaped = true,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if t[i..].starts_with(close) => {
                inner_end = Some(i);
                break;
            }
            None => {}
        }
    }
    let inner_end = inner_end?;
    let rest = t[inner_end + close.len()..].trim_start();
    if !(rest.is_empty() || rest.starts_with('#')) {
        return None;
    }
    let inner = t[inner_start..inner_end].trim();
    Some(match parse_key(inner) {
        Some((name, used)) if open == "[" && used == inner.len() => Header::Table(name),
        _ => Header::Other,
    })
}

/// Parses a bare or quoted key at the start of `s`, returning it and the
/// number of bytes it occupies.
fn parse_key(s: &str) -> Option<(String, usize)> {
    let first = s.chars().next()?;
    if first == '"' || first == '\'' {
        let mut escaped = false;
        for (i, c) in s.char_indices().skip(1) {
            if first == '"' && escaped {
                escaped = false;
            } else if first == '"' && c == '\\' {
                escaped = true;
            } else if c == first {
                return Some((s[1..i].to_owned(), i + 1));
            }
        }
        return None;
    }
    let len = s.bytes().take_while(|&b| is_bare_key_byte(b)).count();
    (len > 0).then(|| (s[..len].to_owned(), len))
}

/// For a `key = value` line, returns the key and the byte offset where the
/// value starts. Dotted keys and anything else return `None`.
fn parse_key_line(line: &str) -> Option<(String, usize)> {
    let indent = line.len() - line.trim_start().len();
    let (key, used) = parse_key(&line[indent..])?;
    let after_key = indent + used;
    let rest = line[after_key..].trim_start();
    let rest = rest.strip_prefix('=')?;
    let value_start = line.len() - rest.trim_start().len();
    Some((key, value_start))
}

/// Replaces the value of a `key = value` line, keeping its indentation, key
/// spelling, spacing and trailing comment.
fn replace_value(line: &str, value_start: usize, value: &str) -> String {
    let after = &line[value_start..];
    let code_len = find_comment(after).unwrap_or(after.len());
    let value_end = value_start + after[..code_len].trim_end().len();
    format!("{}{}{}", &line[..value_start], value, &line[value_end..])
}

/// Byte offset of the `#` that starts a trailing comment, skipping `#` inside
/// basic (`"..."`, with backslash escapes) and literal (`'...'`) strings.
fn find_comment(s: &str) -> Option<usize> {
    let mut quote = None;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if c == '\\' => escaped = true,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '#' => return Some(i),
            None => {}
        }
    }
    None
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
        assert_eq!(config.video.gpu_priority, GpuPriority::Adaptive);
    }

    #[test]
    fn gpu_priority_parses() {
        for (text, want) in [
            ("adaptive", GpuPriority::Adaptive),
            ("normal", GpuPriority::Normal),
            ("high", GpuPriority::High),
        ] {
            let config = Config::from_toml(&format!("[video]\ngpu_priority = \"{text}\"\n"));
            assert_eq!(config.unwrap().video.gpu_priority, want);
        }
        assert!(Config::from_toml("[video]\ngpu_priority = \"realtime\"\n").is_err());
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
    fn zero_quota_means_no_limit() {
        let config = Config::from_toml(
            "[output]
quota_gb = 0
",
        )
        .unwrap();
        assert_eq!(config.output.quota_gb, None);
        let config = Config::from_toml(
            "[output]
quota_gb = 50
",
        )
        .unwrap();
        assert_eq!(config.output.quota_gb, Some(50));
    }

    #[test]
    fn zero_and_empty_unset_optional_settings() {
        let c = Config::from_toml(
            "[video]\nbitrate_kbps = 0\nmax_height = 0\n[audio]\nmicrophone_device = \"\"\n",
        )
        .unwrap();
        assert_eq!(c.video.bitrate_kbps, None);
        assert_eq!(c.video.max_height, None);
        assert_eq!(c.audio.microphone_device, None);
        let c = Config::from_toml("[video]\nbitrate_kbps = 12\nmax_height = 1080\n").unwrap();
        assert_eq!(c.video.bitrate_kbps, Some(1_000));
        assert_eq!(c.video.max_height, Some(1080));
    }

    #[test]
    fn capture_api_parses() {
        assert_eq!(Config::default().video.capture, CaptureApi::Auto);
        for (text, api) in [
            ("auto", CaptureApi::Auto),
            ("wgc", CaptureApi::Wgc),
            ("dda", CaptureApi::Dda),
        ] {
            let c = Config::from_toml(&format!("[video]\ncapture = \"{text}\"\n")).unwrap();
            assert_eq!(c.video.capture, api);
            assert!(
                c.to_toml()
                    .unwrap()
                    .contains(&format!("capture = \"{text}\""))
            );
        }
        assert!(Config::from_toml("[video]\ncapture = \"gdi\"\n").is_err());
    }

    #[test]
    fn save_keys_writes_the_capture_api() {
        let dir = TempDir::new();
        let path =
            dir.write("[video]\n# troubleshooting\ncapture = \"auto\" # was wgc\nfps = 60\n");
        let mut config = Config::load(&path).unwrap();
        config.video.capture = CaptureApi::Dda;
        config.save_keys(&path, &["video.capture"]).unwrap();
        assert_eq!(
            dir.read(),
            "[video]\n# troubleshooting\ncapture = \"dda\" # was wgc\nfps = 60\n"
        );
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
    fn star_last_hotkey_is_optional() {
        assert_eq!(Config::default().hotkeys.star_last, None);
        let config = Config::from_toml("[hotkeys]\nstar_last = \"Alt+F9\"\n").unwrap();
        assert_eq!(config.hotkeys.star_last.unwrap().to_string(), "Alt+F9");
    }

    #[test]
    fn controller_combos_are_off_by_default_and_parse() {
        let h = Config::default().hotkeys;
        assert!(!h.any_controller());
        assert_eq!(h.controller_hold_ms, 500);

        let config = Config::from_toml(
            r#"[hotkeys]
controller_save = "rs+ls"
controller_star_last = "View+Y"
controller_hold_ms = 20
[[hotkeys.extra_clips]]
hotkey = "Alt+F6"
seconds = 15
controller = "View+DPadDown"
[[hotkeys.extra_clips]]
hotkey = "Alt+F5"
seconds = 300
"#,
        )
        .unwrap();
        let h = &config.hotkeys;
        assert!(h.any_controller());
        assert_eq!(h.controller_save.unwrap().to_string(), "LS+RS");
        assert_eq!(h.controller_star_last.unwrap().to_string(), "View+Y");
        assert_eq!(h.controller_add_marker, None);
        assert_eq!(h.controller_hold_ms, MIN_CONTROLLER_HOLD_MS);
        assert_eq!(
            h.extra_clips[0].controller.unwrap().to_string(),
            "View+DPadDown"
        );
        assert_eq!(h.extra_clips[1].controller, None);

        // Round trip, with the canonical names.
        let text = config.to_toml().unwrap();
        assert!(text.contains(r#"controller_save = "LS+RS""#), "{text}");
        assert_eq!(Config::from_toml(&text).unwrap(), config);

        assert!(Config::from_toml("[hotkeys]\ncontroller_save = \"A\"\n").is_err());
        assert!(Config::from_toml("[hotkeys]\ncontroller_save = \"Guide+A\"\n").is_err());
    }

    #[test]
    fn save_keys_writes_a_controller_combo() {
        let dir = std::env::temp_dir().join(format!("rec-config-combo-{}", std::process::id()));
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "# mine\n[hotkeys]\nsave_clip = \"Alt+F8\" # keep\n").unwrap();
        let mut config = Config::load(&path).unwrap();
        config.hotkeys.controller_save = Some("LS+RS".parse().unwrap());
        config
            .save_keys(&path, &["hotkeys.controller_save"])
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep") && text.contains(r#"controller_save = "LS+RS""#));
        assert_eq!(Config::load(&path).unwrap(), config);
        std::fs::remove_dir_all(&dir).unwrap();
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

    /// Temp directory holding a `config.toml`, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "rec-config-keys-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> PathBuf {
            self.0.join("config.toml")
        }

        fn write(&self, text: &str) -> PathBuf {
            std::fs::write(self.path(), text).unwrap();
            self.path()
        }

        fn read(&self) -> String {
            std::fs::read_to_string(self.path()).unwrap()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn save_keys_keeps_comments_order_and_unknown_keys() {
        let dir = TempDir::new();
        let path = dir.write(
            "# my rec config\nfuture_option = 1\n\n[replay]\n# how much to save\nseconds = 30\n\
             post_roll_seconds = 5\nmystery = \"x\"\n\n[future_section]\na = 1\n\n\
             [video]\nquality = \"balanced\"\nfps = 30\n",
        );
        let mut config = Config::load(&path).unwrap();
        config.replay.seconds = 90;
        config.video.quality = Quality::Quality;
        config
            .save_keys(&path, &["replay.seconds", "video.quality"])
            .unwrap();

        assert_eq!(
            dir.read(),
            "# my rec config\nfuture_option = 1\n\n[replay]\n# how much to save\nseconds = 90\n\
             post_roll_seconds = 5\nmystery = \"x\"\n\n[future_section]\na = 1\n\n\
             [video]\nquality = \"quality\"\nfps = 30\n"
        );
        assert_eq!(Config::load(&path).unwrap(), config);
    }

    #[test]
    fn save_keys_only_touches_requested_keys() {
        let dir = TempDir::new();
        let path = dir.write("[replay]\nseconds = 30\n");
        let mut config = Config::default();
        config.video.quality = Quality::Performance;
        config.save_keys(&path, &["video.quality"]).unwrap();

        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.replay.seconds, 30);
        assert_eq!(loaded.video.quality, Quality::Performance);
    }

    #[test]
    fn save_keys_keeps_trailing_comments() {
        let dir = TempDir::new();
        let path = dir.write("[replay]\n  seconds   =   30   # half a minute\n");
        let mut config = Config::default();
        config.replay.seconds = 120;
        config.save_keys(&path, &["replay.seconds"]).unwrap();
        assert_eq!(
            dir.read(),
            "[replay]\n  seconds   =   120   # half a minute\n"
        );
    }

    #[test]
    fn save_keys_inserts_missing_key_into_existing_section() {
        let dir = TempDir::new();
        let path = dir.write("[replay]\nseconds = 30\n# about video\n\n[video]\nfps = 30\n");
        let mut config = Config::default();
        config.replay.post_roll_seconds = 7;
        config
            .save_keys(&path, &["replay.post_roll_seconds"])
            .unwrap();
        assert_eq!(
            dir.read(),
            "[replay]\nseconds = 30\npost_roll_seconds = 7\n# about video\n\n[video]\nfps = 30\n"
        );

        // An empty section gets the key right after its header.
        let path = dir.write("[replay]\n[video]\nfps = 30\n");
        config
            .save_keys(&path, &["replay.post_roll_seconds"])
            .unwrap();
        assert_eq!(
            dir.read(),
            "[replay]\npost_roll_seconds = 7\n[video]\nfps = 30\n"
        );
    }

    #[test]
    fn save_keys_appends_missing_section() {
        let dir = TempDir::new();
        let path = dir.write("# hi\n[replay]\nseconds = 30\n");
        let mut config = Config::default();
        config.video.quality = Quality::Quality;
        config.audio.microphone = true;
        config
            .save_keys(&path, &["video.quality", "audio.microphone"])
            .unwrap();
        assert_eq!(
            dir.read(),
            "# hi\n[replay]\nseconds = 30\n\n[video]\nquality = \"quality\"\n\n[audio]\nmicrophone = true\n"
        );
    }

    #[test]
    fn save_keys_none_removes_the_key() {
        let dir = TempDir::new();
        let path = dir.write("[video]\nfps = 30\nbitrate_kbps = 8000 # custom\ncursor = false\n");
        let config = Config::default();
        config.save_keys(&path, &["video.bitrate_kbps"]).unwrap();
        assert_eq!(dir.read(), "[video]\nfps = 30\ncursor = false\n");
        assert_eq!(Config::load(&path).unwrap().video.bitrate_kbps, None);

        // Removing a key or section that is not there changes nothing.
        let before = dir.read();
        config
            .save_keys(&path, &["video.bitrate_kbps", "output.directory"])
            .unwrap();
        assert_eq!(dir.read(), before);
    }

    #[test]
    fn save_keys_writes_windows_paths() {
        let dir = TempDir::new();
        let path = dir.write("[output]\ndirectory = \"old\" # where clips go\nfilename = \"x\"\n");
        let mut config = Config::default();
        config.output.directory = Some(PathBuf::from(r"D:\Clips\rec"));
        config.save_keys(&path, &["output.directory"]).unwrap();

        let text = dir.read();
        assert!(text.contains("# where clips go"), "{text}");
        let loaded = Config::load(&path).unwrap();
        assert_eq!(
            loaded.output.directory,
            Some(PathBuf::from(r"D:\Clips\rec"))
        );
        assert_eq!(loaded.output.filename, "x");
    }

    #[test]
    fn save_keys_keeps_crlf_line_endings() {
        let dir = TempDir::new();
        let path = dir.write("# c\r\n[replay]\r\nseconds = 30\r\n\r\n[video]\r\nfps = 30\r\n");
        let mut config = Config::default();
        config.replay.seconds = 45;
        config.replay.post_roll_seconds = 3;
        config.audio.microphone = true;
        config
            .save_keys(
                &path,
                &[
                    "replay.seconds",
                    "replay.post_roll_seconds",
                    "audio.microphone",
                ],
            )
            .unwrap();
        assert_eq!(
            dir.read(),
            "# c\r\n[replay]\r\nseconds = 45\r\npost_roll_seconds = 3\r\n\r\n[video]\r\nfps = 30\r\n\r\n[audio]\r\nmicrophone = true\r\n"
        );
    }

    #[test]
    fn save_keys_keeps_a_byte_order_mark() {
        let dir = TempDir::new();
        let path = dir.write("\u{feff}[replay]\nseconds = 30\n");
        let mut config = Config::default();
        config.replay.seconds = 45;
        config.save_keys(&path, &["replay.seconds"]).unwrap();
        assert_eq!(dir.read(), "\u{feff}[replay]\nseconds = 45\n");
    }

    #[test]
    fn save_keys_falls_back_for_dotted_keys() {
        let dir = TempDir::new();
        let path = dir.write(
            "future = 1\nreplay.seconds = 30\nreplay.post_roll_seconds = 9\n[video]\nfps = 30\n",
        );
        let mut config = Config::default();
        config.replay.seconds = 45;
        config.save_keys(&path, &["replay.seconds"]).unwrap();

        let table: toml::Table = toml::from_str(&dir.read()).unwrap();
        assert_eq!(table["future"].as_integer(), Some(1));
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.replay.seconds, 45);
        assert_eq!(loaded.replay.post_roll_seconds, 9);
        assert_eq!(loaded.video.fps, 30);

        // Removing a key that only exists as a dotted key works too.
        let path = dir.write("replay.seconds = 30\nreplay.post_roll_seconds = 9\n");
        Config::default()
            .save_keys(&path, &["replay.post_roll_seconds"])
            .unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.replay.seconds, 30);
        assert_eq!(loaded.replay.post_roll_seconds, 0);
    }

    #[test]
    fn save_keys_falls_back_for_multi_line_values() {
        let dir = TempDir::new();
        let path = dir.write(
            "[behavior]\nprivate_apps = [\n  \"a.exe\",\n  \"b.exe\",\n]\nsave_toast = false\n",
        );
        let mut config = Config::default();
        config.behavior.private_apps = vec!["c.exe".into()];
        config.save_keys(&path, &["behavior.private_apps"]).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.behavior.private_apps, vec!["c.exe".to_string()]);
        assert!(!loaded.behavior.save_toast);
    }

    #[test]
    fn save_keys_creates_a_missing_file() {
        let dir = TempDir::new();
        let path = dir.path();
        let mut config = Config::default();
        config.replay.seconds = 75;
        config.save_keys(&path, &["replay.seconds"]).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
    }

    #[test]
    fn save_keys_leaves_an_unparseable_file_alone() {
        let dir = TempDir::new();
        let broken = "[replay\nseconds = = 30\n# keep me\n";
        let path = dir.write(broken);
        let err = Config::default()
            .save_keys(&path, &["replay.seconds"])
            .unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
        assert_eq!(dir.read(), broken);
        assert!(!path.with_extension("toml.tmp").exists());
    }

    #[test]
    fn save_keys_rejects_bad_key_names() {
        let dir = TempDir::new();
        let path = dir.write("[replay]\nseconds = 30\n");
        for key in ["seconds", "replay.", ".seconds", "a.b.c", "replay.sec onds"] {
            let err = Config::default().save_keys(&path, &[key]).unwrap_err();
            assert!(matches!(err, ConfigError::InvalidKey(_)), "{key}");
        }
        assert_eq!(dir.read(), "[replay]\nseconds = 30\n");
    }
}
