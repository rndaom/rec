//! The quick panel's content and behaviour, apart from drawing: which rows
//! it shows, what keys and clicks do to them, and the values they step
//! through. It holds no Win32 state, so all of it is unit-tested;
//! [`crate::overlay`] draws it and `app` applies the [`Command`]s.
//!
//! The panel is for changing things mid-game without leaving the game, so
//! every row works from the keyboard alone (arrows or WASD, Enter/Space,
//! Esc) as well as with the mouse, and every change takes effect at once
//! without throwing the replay buffer away (see `app` for how each one is
//! applied).

use rec_core::config::Quality;

use crate::session::AudioSwitch;

/// Clip lengths the panel steps through, in seconds. A length set in the
/// config file that is not listed stays as it is until changed.
pub const LENGTHS: [u32; 11] = [15, 30, 45, 60, 90, 120, 180, 300, 600, 900, 1200];

const QUALITIES: [Quality; 3] = [Quality::Performance, Quality::Balanced, Quality::Quality];

/// The quality the config asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QualityChoice {
    Preset(Quality),
    /// `video.bitrate_kbps` overrides the presets. Stepping from it picks a
    /// preset (and drops the override).
    Custom,
}

/// What an audio row switches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioTarget {
    /// A source setting (saved to the config); mutes its input live.
    Switch(AudioSwitch),
    /// An input that stands for several settings at once (system loopback
    /// when per-app capture is unavailable): muted for this session only.
    Shared(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioRow {
    pub target: AudioTarget,
    pub name: &'static str,
    pub on: bool,
    /// Why the switch does not affect the current recording, if it doesn't.
    pub note: Option<&'static str>,
}

/// The clip saved last in this run, which "Star last clip" stars.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastClip {
    /// File name without the extension.
    pub name: String,
    pub starred: bool,
}

/// A save with its own length (`hotkeys.extra_clips`), shown as a row
/// under "Save clip".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtraClip {
    pub seconds: u32,
    /// Its hotkey, and controller combo if it has one: "Alt+F6 · View+DPadDown".
    pub keys: String,
}

/// Everything the panel shows, rebuilt by `app` whenever it changes.
#[derive(Clone, Debug, PartialEq)]
pub struct View {
    /// "Recording Apex Legends", "Paused", "Waiting for a game"…
    pub title: String,
    /// Buffer fill and encoder, or what is wrong.
    pub status: String,
    /// `status` describes a problem.
    pub alert: bool,
    pub recording: bool,
    pub paused: bool,
    pub seconds: u32,
    pub quality: QualityChoice,
    /// Video bitrate in use (or that would be used), for the size estimate.
    pub bitrate_kbps: Option<u32>,
    pub memory_cap_mb: u32,
    pub audio: Vec<AudioRow>,
    pub folder: String,
    /// The save hotkey, for the footer.
    pub save_hotkey: String,
    /// Saves of other lengths, in config order.
    pub extra_clips: Vec<ExtraClip>,
    /// `None` until a clip is saved.
    pub last_clip: Option<LastClip>,
}

/// A row of the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    Save,
    /// Save with a length of its own: an index into [`View::extra_clips`].
    SaveExtra(usize),
    /// Star the clip saved last, so quota pruning keeps it.
    Star,
    Length,
    Quality,
    /// An index into [`View::audio`].
    Audio(usize),
    Folder,
    OpenFolder,
    Pause,
}

impl Item {
    /// Whether Left/Right change the row's value.
    pub fn steps(self) -> bool {
        matches!(self, Item::Length | Item::Quality | Item::Audio(_))
    }

    /// The group the row belongs to; groups are drawn with a gap between.
    fn group(self) -> u8 {
        match self {
            Item::Save | Item::SaveExtra(_) | Item::Star => 0,
            Item::Length | Item::Quality => 1,
            Item::Audio(_) => 2,
            Item::Folder | Item::OpenFolder | Item::Pause => 3,
        }
    }
}

/// What the user asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Save,
    /// Save the last this many seconds.
    SaveSeconds(u32),
    StarLast,
    SetSeconds(u32),
    SetQuality(Quality),
    SetAudio(AudioTarget, bool),
    PickFolder,
    OpenFolder,
    TogglePause,
    Close,
}

/// A key the panel reacts to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    /// Enter or Space.
    Activate,
    /// Esc.
    Close,
}

impl Key {
    /// Maps a virtual-key code; WASD work like the arrows, since a player's
    /// hand is usually there.
    pub fn from_vk(vk: u16, shift: bool) -> Option<Self> {
        Some(match vk {
            0x26 | 0x57 => Key::Up,    // VK_UP, W
            0x28 | 0x53 => Key::Down,  // VK_DOWN, S
            0x25 | 0x41 => Key::Left,  // VK_LEFT, A
            0x27 | 0x44 => Key::Right, // VK_RIGHT, D
            0x09 if shift => Key::Up,  // Shift+Tab
            0x09 => Key::Down,         // Tab
            0x24 => Key::Home,
            0x23 => Key::End,
            0x0D | 0x20 => Key::Activate, // Enter, Space
            0x1B => Key::Close,
            _ => return None,
        })
    }
}

/// The next listed length after `current` in direction `dir` (+1 or -1),
/// or `None` at the end of the list.
pub fn step_length(current: u32, dir: i32) -> Option<u32> {
    if dir > 0 {
        LENGTHS.iter().copied().find(|&s| s > current)
    } else {
        LENGTHS.iter().rev().copied().find(|&s| s < current)
    }
}

pub fn step_quality(current: QualityChoice, dir: i32) -> Option<Quality> {
    let QualityChoice::Preset(q) = current else {
        return Some(Quality::Balanced);
    };
    let i = QUALITIES.iter().position(|&p| p == q)?;
    let next = i.checked_add_signed(dir as isize)?;
    QUALITIES.get(next).copied()
}

pub fn quality_name(q: QualityChoice) -> &'static str {
    match q {
        QualityChoice::Preset(Quality::Performance) => "Performance",
        QualityChoice::Preset(Quality::Balanced) => "Balanced",
        QualityChoice::Preset(Quality::Quality) => "Quality",
        QualityChoice::Custom => "Custom",
    }
}

/// Seconds as the panel shows them: "45 s", "2 min", "1 min 30 s".
pub fn format_seconds(s: u32) -> String {
    match (s / 60, s % 60) {
        (0, s) => format!("{s} s"),
        (m, 0) => format!("{m} min"),
        (m, s) => format!("{m} min {s} s"),
    }
}

/// kbit/s as whole Mbit/s, rounded (5931 -> 6), as shown everywhere.
pub fn mbit(kbps: u32) -> u32 {
    (kbps + 500) / 1000
}

/// Approximate buffer size for `seconds` of video at `kbps` (audio is a
/// few percent on top and ignored), in MB.
pub fn estimate_mb(seconds: u32, kbps: u32) -> u64 {
    u64::from(seconds) * u64::from(kbps) / 8 / 1024
}

/// The panel's interaction state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Panel {
    selected: usize,
}

impl Panel {
    /// Opens with "Save clip" selected while recording, else with the first
    /// row that does something.
    pub fn new(view: &View) -> Self {
        let mut panel = Self { selected: 0 };
        panel.fix_selection(view, 1);
        panel
    }

    pub fn items(view: &View) -> Vec<Item> {
        let mut items = vec![Item::Save];
        items.extend((0..view.extra_clips.len()).map(Item::SaveExtra));
        items.extend([Item::Star, Item::Length, Item::Quality]);
        items.extend((0..view.audio.len()).map(Item::Audio));
        items.extend([Item::Folder, Item::OpenFolder, Item::Pause]);
        items
    }

    /// Rows that can't do anything right now; skipped by the keyboard.
    pub fn enabled(view: &View, item: Item) -> bool {
        match item {
            Item::Save | Item::SaveExtra(_) => view.recording,
            // Starring twice would do nothing.
            Item::Star => view.last_clip.as_ref().is_some_and(|c| !c.starred),
            _ => true,
        }
    }

    pub fn selected(&self, view: &View) -> Option<Item> {
        Self::items(view).get(self.selected).copied()
    }

    /// Keeps the selection on an enabled row after the view changed,
    /// searching in direction `dir`.
    pub fn fix_selection(&mut self, view: &View, dir: i32) {
        let items = Self::items(view);
        self.selected = self.selected.min(items.len().saturating_sub(1));
        if items
            .get(self.selected)
            .is_some_and(|&i| Self::enabled(view, i))
        {
            return;
        }
        if let Some(i) = self.next_enabled(&items, view, dir) {
            self.selected = i;
        } else if let Some(i) = self.next_enabled(&items, view, -dir) {
            self.selected = i;
        }
    }

    fn next_enabled(&self, items: &[Item], view: &View, dir: i32) -> Option<usize> {
        let mut i = self.selected;
        loop {
            i = i.checked_add_signed(dir as isize)?;
            let item = *items.get(i)?;
            if Self::enabled(view, item) {
                return Some(i);
            }
        }
    }

    fn select_edge(&mut self, view: &View, last: bool) {
        let items = Self::items(view);
        let found = if last {
            items.iter().rposition(|&i| Self::enabled(view, i))
        } else {
            items.iter().position(|&i| Self::enabled(view, i))
        };
        if let Some(i) = found {
            self.selected = i;
        }
    }

    pub fn key(&mut self, view: &View, key: Key) -> Option<Command> {
        let items = Self::items(view);
        match key {
            Key::Up | Key::Down => {
                let dir = if key == Key::Up { -1 } else { 1 };
                if let Some(i) = self.next_enabled(&items, view, dir) {
                    self.selected = i;
                } else {
                    // Wrap around, so the bottom row is one key away.
                    self.select_edge(view, dir < 0);
                }
                None
            }
            Key::Home => {
                self.select_edge(view, false);
                None
            }
            Key::End => {
                self.select_edge(view, true);
                None
            }
            Key::Left | Key::Right => {
                let dir = if key == Key::Left { -1 } else { 1 };
                change(view, *items.get(self.selected)?, dir)
            }
            Key::Activate => activate(view, *items.get(self.selected)?),
            Key::Close => Some(Command::Close),
        }
    }

    /// Hovering selects the row under the mouse.
    pub fn hover(&mut self, view: &View, row: usize) {
        let items = Self::items(view);
        if items.get(row).is_some_and(|&i| Self::enabled(view, i)) {
            self.selected = row;
        }
    }

    /// A click on `row`; `side` is -1 or +1 for the left or right half of a
    /// stepper's value, 0 elsewhere.
    pub fn click(&mut self, view: &View, row: usize, side: i32) -> Option<Command> {
        let item = *Self::items(view).get(row)?;
        if !Self::enabled(view, item) {
            return None;
        }
        self.selected = row;
        match item {
            Item::Length | Item::Quality if side != 0 => change(view, item, side),
            // Clicking a stepper's label moves it forward.
            Item::Length | Item::Quality => change(view, item, 1),
            _ => activate(view, item),
        }
    }

    /// The mouse wheel over `row` (+1 up, -1 down) steps its value.
    pub fn wheel(&mut self, view: &View, row: usize, dir: i32) -> Option<Command> {
        let item = *Self::items(view).get(row)?;
        if !item.steps() || !Self::enabled(view, item) {
            return None;
        }
        self.selected = row;
        change(view, item, dir)
    }
}

fn change(view: &View, item: Item, dir: i32) -> Option<Command> {
    match item {
        Item::Length => step_length(view.seconds, dir).map(Command::SetSeconds),
        Item::Quality => step_quality(view.quality, dir).map(Command::SetQuality),
        Item::Audio(i) => {
            let row = view.audio.get(i)?;
            let on = dir > 0;
            (row.on != on).then_some(Command::SetAudio(row.target, on))
        }
        _ => None,
    }
}

fn activate(view: &View, item: Item) -> Option<Command> {
    match item {
        Item::Save => Some(Command::Save),
        Item::SaveExtra(i) => Some(Command::SaveSeconds(view.extra_clips.get(i)?.seconds)),
        Item::Star => Some(Command::StarLast),
        Item::Length | Item::Quality => change(view, item, 1),
        Item::Audio(i) => {
            let row = view.audio.get(i)?;
            Some(Command::SetAudio(row.target, !row.on))
        }
        Item::Folder => Some(Command::PickFolder),
        Item::OpenFolder => Some(Command::OpenFolder),
        Item::Pause => Some(Command::TogglePause),
    }
}

// ---- layout ------------------------------------------------------------------

/// Sizes in device-independent pixels (1/96 inch); the overlay scales them
/// for the monitor's DPI.
pub mod size {
    pub const WIDTH: f32 = 400.0;
    pub const PAD: f32 = 16.0;
    pub const HEADER: f32 = 64.0;
    pub const ROW: f32 = 34.0;
    /// Space between groups of rows.
    pub const GAP: f32 = 10.0;
    pub const FOOTER: f32 = 36.0;
    pub const RADIUS: f32 = 12.0;
    /// Width of the value column (steppers, switches, the folder path).
    pub const VALUE: f32 = 190.0;
}

/// Where each row is, for drawing and hit-testing.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub width: f32,
    pub height: f32,
    /// Top of each row, in [`Panel::items`] order.
    pub rows: Vec<f32>,
    /// Tops of the gaps between groups (a divider is drawn there).
    pub dividers: Vec<f32>,
    pub footer: f32,
}

impl Layout {
    pub fn new(view: &View) -> Self {
        let mut y = size::HEADER;
        let mut rows = Vec::new();
        let mut dividers = vec![y];
        y += size::GAP;
        let mut group = None;
        for item in Panel::items(view) {
            if group.is_some_and(|g| g != item.group()) {
                dividers.push(y);
                y += size::GAP;
            }
            group = Some(item.group());
            rows.push(y);
            y += size::ROW;
        }
        y += size::GAP;
        dividers.push(y);
        Self {
            width: size::WIDTH,
            height: y + size::FOOTER,
            rows,
            dividers,
            footer: y,
        }
    }

    /// The row at `(x, y)` and, for a stepper's value, which half was hit.
    pub fn hit(&self, x: f32, y: f32) -> Option<(usize, i32)> {
        if !(0.0..self.width).contains(&x) {
            return None;
        }
        let row = self
            .rows
            .iter()
            .position(|&top| (top..top + size::ROW).contains(&y))?;
        let value_left = self.width - size::PAD - size::VALUE;
        let side = if x < value_left {
            0
        } else if x < value_left + size::VALUE / 2.0 {
            -1
        } else {
            1
        };
        Some((row, side))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> View {
        View {
            title: "Recording Game".into(),
            status: "60 s buffered".into(),
            alert: false,
            recording: true,
            paused: false,
            seconds: 60,
            quality: QualityChoice::Preset(Quality::Balanced),
            bitrate_kbps: Some(18_000),
            memory_cap_mb: 1024,
            audio: vec![
                AudioRow {
                    target: AudioTarget::Switch(AudioSwitch::Game),
                    name: "Game",
                    on: true,
                    note: None,
                },
                AudioRow {
                    target: AudioTarget::Switch(AudioSwitch::Microphone),
                    name: "Microphone",
                    on: false,
                    note: None,
                },
            ],
            folder: r"C:\Users\me\Videos\rec".into(),
            save_hotkey: "Alt+F8".into(),
            extra_clips: Vec::new(),
            last_clip: None,
        }
    }

    #[test]
    fn extra_clips_are_save_rows_after_save_clip() {
        let mut v = view();
        v.extra_clips = vec![
            ExtraClip {
                seconds: 15,
                keys: "Alt+F6 · View+DPadDown".into(),
            },
            ExtraClip {
                seconds: 300,
                keys: "Alt+F5".into(),
            },
        ];
        let items = Panel::items(&v);
        assert_eq!(
            items[..5],
            [
                Item::Save,
                Item::SaveExtra(0),
                Item::SaveExtra(1),
                Item::Star,
                Item::Length
            ]
        );
        let mut p = Panel::new(&v);
        p.key(&v, Key::Down);
        assert_eq!(p.selected(&v), Some(Item::SaveExtra(0)));
        assert_eq!(p.key(&v, Key::Activate), Some(Command::SaveSeconds(15)));
        // Not steppers.
        assert_eq!(p.key(&v, Key::Right), None);
        assert_eq!(p.wheel(&v, 2, 1), None);
        assert_eq!(p.click(&v, 2, 1), Some(Command::SaveSeconds(300)));
        // Star (no clip yet) is skipped on the way to Length.
        p.key(&v, Key::Down);
        assert_eq!(p.selected(&v), Some(Item::Length));

        // One group with Save and Star: no gap between them.
        let layout = Layout::new(&v);
        assert_eq!(layout.rows[3], layout.rows[0] + 3.0 * size::ROW);
        assert_eq!(layout.rows[4], layout.rows[3] + size::ROW + size::GAP);

        // Like Save, they need a recording.
        v.recording = false;
        assert!(!Panel::enabled(&v, Item::SaveExtra(0)));
        assert_eq!(Panel::new(&v).selected(&v), Some(Item::Length));
        assert_eq!(Panel::new(&v).click(&v, 1, 0), None);
    }

    #[test]
    fn lengths_step_through_the_list_and_stop_at_the_ends() {
        assert_eq!(step_length(60, 1), Some(90));
        assert_eq!(step_length(60, -1), Some(45));
        // An unlisted length steps to its neighbours.
        assert_eq!(step_length(75, 1), Some(90));
        assert_eq!(step_length(75, -1), Some(60));
        assert_eq!(step_length(1200, 1), None);
        assert_eq!(step_length(15, -1), None);
        assert_eq!(step_length(5, 1), Some(15));
    }

    #[test]
    fn quality_steps_and_custom_goes_to_a_preset() {
        let q = |q| QualityChoice::Preset(q);
        assert_eq!(
            step_quality(q(Quality::Balanced), 1),
            Some(Quality::Quality)
        );
        assert_eq!(step_quality(q(Quality::Quality), 1), None);
        assert_eq!(step_quality(q(Quality::Performance), -1), None);
        assert_eq!(
            step_quality(QualityChoice::Custom, -1),
            Some(Quality::Balanced)
        );
    }

    #[test]
    fn formats_lengths() {
        assert_eq!(format_seconds(45), "45 s");
        assert_eq!(format_seconds(120), "2 min");
        assert_eq!(format_seconds(90), "1 min 30 s");
        assert_eq!(estimate_mb(60, 18_000), 131);
        assert_eq!(mbit(5_931), 6);
        assert_eq!(mbit(18_000), 18);
    }

    #[test]
    fn keyboard_moves_wraps_and_changes_values() {
        let v = view();
        let mut p = Panel::new(&v);
        assert_eq!(p.selected(&v), Some(Item::Save));
        assert_eq!(p.key(&v, Key::Activate), Some(Command::Save));
        p.key(&v, Key::Down);
        assert_eq!(p.selected(&v), Some(Item::Length));
        assert_eq!(p.key(&v, Key::Right), Some(Command::SetSeconds(90)));
        assert_eq!(p.key(&v, Key::Left), Some(Command::SetSeconds(45)));
        p.key(&v, Key::Down);
        assert_eq!(
            p.key(&v, Key::Left),
            Some(Command::SetQuality(Quality::Performance))
        );
        p.key(&v, Key::Up);
        p.key(&v, Key::Up);
        // Wraps from the top to the bottom.
        p.key(&v, Key::Up);
        assert_eq!(p.selected(&v), Some(Item::Pause));
        assert_eq!(p.key(&v, Key::Activate), Some(Command::TogglePause));
        p.key(&v, Key::Down);
        assert_eq!(p.selected(&v), Some(Item::Save));
        assert_eq!(p.key(&v, Key::Close), Some(Command::Close));
    }

    #[test]
    fn audio_rows_toggle_and_arrows_set_a_side() {
        let v = view();
        let mut p = Panel::new(&v);
        p.key(&v, Key::Home);
        for _ in 0..4 {
            p.key(&v, Key::Down);
        }
        assert_eq!(p.selected(&v), Some(Item::Audio(1)));
        let mic = AudioTarget::Switch(AudioSwitch::Microphone);
        assert_eq!(p.key(&v, Key::Activate), Some(Command::SetAudio(mic, true)));
        assert_eq!(p.key(&v, Key::Right), Some(Command::SetAudio(mic, true)));
        // Already off.
        assert_eq!(p.key(&v, Key::Left), None);
    }

    #[test]
    fn disabled_rows_are_skipped() {
        let mut v = view();
        v.recording = false;
        let mut p = Panel::new(&v);
        assert_eq!(p.selected(&v), Some(Item::Length));
        p.key(&v, Key::Up);
        // Wrapped past the disabled Save row.
        assert_eq!(p.selected(&v), Some(Item::Pause));
        p.key(&v, Key::Home);
        assert_eq!(p.selected(&v), Some(Item::Length));
        let save_row = 0;
        assert_eq!(p.click(&v, save_row, 0), None);
        p.hover(&v, save_row);
        assert_eq!(p.selected(&v), Some(Item::Length));
    }

    #[test]
    fn selection_survives_rows_disappearing() {
        let mut v = view();
        let mut p = Panel::new(&v);
        p.key(&v, Key::End);
        v.audio.pop();
        p.fix_selection(&v, 1);
        assert_eq!(p.selected(&v), Some(Item::Pause));
    }

    #[test]
    fn layout_hit_tests_rows_and_stepper_halves() {
        let v = view();
        let layout = Layout::new(&v);
        let items = Panel::items(&v);
        assert_eq!(layout.rows.len(), items.len());
        // Header, then groups separated by gaps.
        assert_eq!(layout.rows[0], size::HEADER + size::GAP);
        // Save and Star share a group.
        assert_eq!(layout.rows[1], layout.rows[0] + size::ROW);
        assert_eq!(layout.rows[2], layout.rows[1] + size::ROW + size::GAP);
        assert_eq!(layout.rows[3], layout.rows[2] + size::ROW);
        assert!(layout.height > *layout.rows.last().unwrap() + size::ROW);

        let mid = |row: usize| layout.rows[row] + size::ROW / 2.0;
        assert_eq!(layout.hit(20.0, mid(2)), Some((2, 0)));
        let value_left = size::WIDTH - size::PAD - size::VALUE;
        assert_eq!(layout.hit(value_left + 10.0, mid(2)), Some((2, -1)));
        assert_eq!(layout.hit(size::WIDTH - 20.0, mid(2)), Some((2, 1)));
        // The header, a gap and outside the panel hit nothing.
        assert_eq!(layout.hit(20.0, 10.0), None);
        assert_eq!(layout.hit(20.0, layout.rows[2] - 2.0), None);
        assert_eq!(layout.hit(-1.0, mid(2)), None);
    }

    #[test]
    fn clicks_and_wheel() {
        let v = view();
        let mut p = Panel::new(&v);
        let length = 2;
        assert_eq!(p.click(&v, length, -1), Some(Command::SetSeconds(45)));
        assert_eq!(p.click(&v, length, 0), Some(Command::SetSeconds(90)));
        assert_eq!(p.wheel(&v, length, 1), Some(Command::SetSeconds(90)));
        assert_eq!(p.wheel(&v, 0, 1), None);
        let folder = Panel::items(&v)
            .iter()
            .position(|&i| i == Item::Folder)
            .unwrap();
        assert_eq!(p.click(&v, folder, 1), Some(Command::PickFolder));
        assert_eq!(p.selected(&v), Some(Item::Folder));
    }

    #[test]
    fn star_works_once_a_clip_is_saved_until_it_is_starred() {
        let mut v = view();
        let mut p = Panel::new(&v);
        let star = 1;
        assert_eq!(Panel::items(&v)[star], Item::Star);
        // Nothing saved yet: skipped by the keyboard, clicks do nothing.
        assert!(!Panel::enabled(&v, Item::Star));
        p.key(&v, Key::Down);
        assert_eq!(p.selected(&v), Some(Item::Length));
        assert_eq!(p.click(&v, star, 0), None);

        v.last_clip = Some(LastClip {
            name: "TF2_2026-10-18_21-04-03".into(),
            starred: false,
        });
        p.key(&v, Key::Home);
        p.key(&v, Key::Down);
        assert_eq!(p.selected(&v), Some(Item::Star));
        assert_eq!(p.key(&v, Key::Activate), Some(Command::StarLast));
        assert_eq!(p.click(&v, star, 1), Some(Command::StarLast));
        // Not a stepper.
        assert_eq!(p.key(&v, Key::Right), None);
        assert_eq!(p.wheel(&v, star, 1), None);

        // Once starred, the selection moves off it.
        v.last_clip.as_mut().unwrap().starred = true;
        p.fix_selection(&v, 1);
        assert_eq!(p.selected(&v), Some(Item::Length));
        assert_eq!(p.click(&v, star, 0), None);

        // Not recording, with a clip to star: it is the first live row.
        v.recording = false;
        v.last_clip.as_mut().unwrap().starred = false;
        assert_eq!(Panel::new(&v).selected(&v), Some(Item::Star));
    }

    #[test]
    fn keys_map_from_virtual_keys() {
        assert_eq!(Key::from_vk(0x57, false), Some(Key::Up));
        assert_eq!(Key::from_vk(0x09, true), Some(Key::Up));
        assert_eq!(Key::from_vk(0x09, false), Some(Key::Down));
        assert_eq!(Key::from_vk(0x20, false), Some(Key::Activate));
        assert_eq!(Key::from_vk(0x1B, false), Some(Key::Close));
        assert_eq!(Key::from_vk(0x51, false), None);
    }
}
