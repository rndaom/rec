//! Hotkey chords such as `Alt+F8` or `Ctrl+Shift+F10`.
//!
//! Parsing and validation live here so the CLI, the settings UI and `recd`
//! agree on what a valid hotkey is. Mapping to Win32 virtual-key codes happens
//! in `recd`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
}

impl Modifiers {
    pub fn any(self) -> bool {
        self.ctrl || self.alt || self.shift || self.win
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// `A`..=`Z`, stored uppercase.
    Letter(char),
    /// `0`..=`9` on the main row.
    Digit(u8),
    /// `F1`..=`F24`.
    Function(u8),
    /// `Num0`..=`Num9`.
    Numpad(u8),
    Named(NamedKey),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NamedKey {
    Space,
    Tab,
    Enter,
    Escape,
    Backspace,
    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
    PrintScreen,
    ScrollLock,
    Pause,
}

const NAMED_KEYS: &[(NamedKey, &str, &[&str])] = &[
    (NamedKey::Space, "Space", &[]),
    (NamedKey::Tab, "Tab", &[]),
    (NamedKey::Enter, "Enter", &["return"]),
    (NamedKey::Escape, "Esc", &["escape"]),
    (NamedKey::Backspace, "Backspace", &[]),
    (NamedKey::Insert, "Insert", &["ins"]),
    (NamedKey::Delete, "Delete", &["del"]),
    (NamedKey::Home, "Home", &[]),
    (NamedKey::End, "End", &[]),
    (NamedKey::PageUp, "PageUp", &["pgup"]),
    (NamedKey::PageDown, "PageDown", &["pgdn"]),
    (NamedKey::Up, "Up", &[]),
    (NamedKey::Down, "Down", &[]),
    (NamedKey::Left, "Left", &[]),
    (NamedKey::Right, "Right", &[]),
    (NamedKey::PrintScreen, "PrintScreen", &["prtsc", "printscr"]),
    (NamedKey::ScrollLock, "ScrollLock", &[]),
    (NamedKey::Pause, "Pause", &["break"]),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Hotkey {
    pub modifiers: Modifiers,
    pub key: Key,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HotkeyError {
    #[error("hotkey is empty")]
    Empty,
    #[error("unknown key or modifier `{0}`")]
    UnknownKey(String),
    #[error("modifier `{0}` appears twice")]
    DuplicateModifier(String),
    #[error("hotkey has modifiers but no key")]
    MissingKey,
    #[error("hotkey has more than one key (`{0}` and `{1}`)")]
    MultipleKeys(String, String),
    #[error(
        "`{0}` needs at least one modifier (Ctrl, Alt, Shift or Win) so normal typing doesn't trigger it"
    )]
    NeedsModifier(String),
    #[error("F12 is reserved by Windows for debuggers and can't be used as a global hotkey")]
    Reserved,
}

impl Hotkey {
    pub const fn new(modifiers: Modifiers, key: Key) -> Self {
        Self { modifiers, key }
    }

    /// Default hotkeys of other common capture tools. Binding one of these
    /// usually fails or fires both apps, so the UI warns about it.
    pub fn known_conflict(&self) -> Option<&'static str> {
        const CONFLICTS: &[(&str, &str)] = &[
            ("Alt+F9", "NVIDIA App: start/stop recording"),
            ("Alt+F10", "NVIDIA App: save instant replay"),
            ("Alt+Shift+F10", "NVIDIA App: toggle instant replay"),
            ("Alt+Z", "NVIDIA App: overlay"),
            ("Alt+R", "AMD Software: overlay"),
            ("Win+G", "Xbox Game Bar"),
            ("Alt+Win+G", "Xbox Game Bar: record what happened"),
            ("Alt+Win+R", "Xbox Game Bar: start/stop recording"),
        ];
        CONFLICTS
            .iter()
            .find(|(chord, _)| chord.parse::<Hotkey>().is_ok_and(|h| h == *self))
            .map(|(_, owner)| *owner)
    }
}

fn parse_key(token: &str) -> Option<Key> {
    let lower = token.to_ascii_lowercase();
    let mut chars = lower.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphabetic() {
            return Some(Key::Letter(c.to_ascii_uppercase()));
        }
        if let Some(d) = c.to_digit(10) {
            return Some(Key::Digit(d as u8));
        }
    }
    if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
        return (1..=24).contains(&n).then_some(Key::Function(n));
    }
    if let Some(n) = lower
        .strip_prefix("numpad")
        .or_else(|| lower.strip_prefix("num"))
        .and_then(|n| n.parse::<u8>().ok())
    {
        return (n <= 9).then_some(Key::Numpad(n));
    }
    NAMED_KEYS
        .iter()
        .find(|(_, name, aliases)| name.eq_ignore_ascii_case(&lower) || aliases.contains(&&*lower))
        .map(|(key, _, _)| Key::Named(*key))
}

impl FromStr for Hotkey {
    type Err = HotkeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err(HotkeyError::Empty);
        }

        let mut modifiers = Modifiers::default();
        let mut key: Option<(Key, &str)> = None;
        for token in s.split('+').map(str::trim) {
            let slot = match token.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => Some(&mut modifiers.ctrl),
                "alt" => Some(&mut modifiers.alt),
                "shift" => Some(&mut modifiers.shift),
                "win" | "windows" | "super" => Some(&mut modifiers.win),
                _ => None,
            };
            if let Some(slot) = slot {
                if *slot {
                    return Err(HotkeyError::DuplicateModifier(token.to_string()));
                }
                *slot = true;
                continue;
            }

            let parsed =
                parse_key(token).ok_or_else(|| HotkeyError::UnknownKey(token.to_string()))?;
            if let Some((_, first)) = key {
                return Err(HotkeyError::MultipleKeys(
                    first.to_string(),
                    token.to_string(),
                ));
            }
            key = Some((parsed, token));
        }

        let (key, token) = key.ok_or(HotkeyError::MissingKey)?;
        if key == Key::Function(12) {
            return Err(HotkeyError::Reserved);
        }
        let standalone_ok = matches!(
            key,
            Key::Function(_) | Key::Named(NamedKey::Pause | NamedKey::ScrollLock)
        );
        if !modifiers.any() && !standalone_ok {
            return Err(HotkeyError::NeedsModifier(token.to_string()));
        }
        Ok(Self { modifiers, key })
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Key::Letter(c) => write!(f, "{c}"),
            Key::Digit(d) => write!(f, "{d}"),
            Key::Function(n) => write!(f, "F{n}"),
            Key::Numpad(n) => write!(f, "Num{n}"),
            Key::Named(named) => {
                let name = NAMED_KEYS
                    .iter()
                    .find(|(k, _, _)| k == named)
                    .map_or("?", |(_, name, _)| *name);
                f.write_str(name)
            }
        }
    }
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = self.modifiers;
        for (on, name) in [
            (m.ctrl, "Ctrl+"),
            (m.alt, "Alt+"),
            (m.shift, "Shift+"),
            (m.win, "Win+"),
        ] {
            if on {
                f.write_str(name)?;
            }
        }
        write!(f, "{}", self.key)
    }
}

impl TryFrom<String> for Hotkey {
    type Error = HotkeyError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Hotkey> for String {
    fn from(h: Hotkey) -> Self {
        h.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hk(s: &str) -> Hotkey {
        s.parse().unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn parses_and_canonicalises() {
        assert_eq!(hk("alt+f8").to_string(), "Alt+F8");
        assert_eq!(hk(" shift + CONTROL + s ").to_string(), "Ctrl+Shift+S");
        assert_eq!(hk("Win+Alt+PrtSc").to_string(), "Alt+Win+PrintScreen");
        assert_eq!(hk("Ctrl+Numpad5").to_string(), "Ctrl+Num5");
        assert_eq!(hk("Alt+1").key, Key::Digit(1));
    }

    #[test]
    fn standalone_function_keys_allowed() {
        assert_eq!(hk("F13").to_string(), "F13");
        assert_eq!(hk("Pause").to_string(), "Pause");
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!("".parse::<Hotkey>(), Err(HotkeyError::Empty));
        assert_eq!("Ctrl+Alt".parse::<Hotkey>(), Err(HotkeyError::MissingKey));
        assert_eq!(
            "Alt+Alt+S".parse::<Hotkey>(),
            Err(HotkeyError::DuplicateModifier("Alt".into()))
        );
        assert!(matches!(
            "Alt+S+D".parse::<Hotkey>(),
            Err(HotkeyError::MultipleKeys(..))
        ));
        assert!(matches!(
            "Alt+Banana".parse::<Hotkey>(),
            Err(HotkeyError::UnknownKey(_))
        ));
        assert!(matches!(
            "S".parse::<Hotkey>(),
            Err(HotkeyError::NeedsModifier(_))
        ));
        assert!(matches!(
            "F25".parse::<Hotkey>(),
            Err(HotkeyError::UnknownKey(_))
        ));
        assert_eq!("Ctrl+F12".parse::<Hotkey>(), Err(HotkeyError::Reserved));
    }

    #[test]
    fn detects_known_conflicts_regardless_of_order() {
        assert!(hk("Shift+Alt+F10").known_conflict().is_some());
        assert!(hk("Alt+Win+G").known_conflict().is_some());
        assert_eq!(hk("Alt+F8").known_conflict(), None);
    }

    #[test]
    fn serde_uses_strings() {
        let json = serde_json::to_string(&hk("ctrl+alt+s")).unwrap();
        assert_eq!(json, "\"Ctrl+Alt+S\"");
        assert!(serde_json::from_str::<Hotkey>("\"S\"").is_err());
    }
}
