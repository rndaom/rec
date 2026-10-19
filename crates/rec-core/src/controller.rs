//! Controller button combos such as `LS+RS` or `View+DPadDown`.
//!
//! Parsing and validation live here, next to keyboard hotkeys, so the CLI,
//! the settings UI and `recd` agree on what a valid combo is. `recd` reads
//! the buttons with XInput (see `recd/src/controller.rs`). The bit values are
//! XInput's `XINPUT_GAMEPAD_*` button flags, with the two analog triggers
//! added above them, so a reading maps to a [`Combo`] without a table.
//!
//! The Xbox (Guide) button is not available: public XInput doesn't report
//! it, and Xbox Game Bar owns it.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Button {
    LeftBumper,
    RightBumper,
    LeftTrigger,
    RightTrigger,
    View,
    Menu,
    LeftStick,
    RightStick,
    DPadUp,
    DPadDown,
    DPadLeft,
    DPadRight,
    A,
    B,
    X,
    Y,
}

/// Every button with its bit, its canonical name and other accepted names
/// (compared lowercase, ignoring `-`, `_` and spaces), in display order.
/// The aliases cover the older Xbox 360 names and PlayStation names, for
/// pads that reach XInput through Steam Input or DS4Windows.
const BUTTONS: &[(Button, u32, &str, &[&str])] = &[
    (
        Button::LeftBumper,
        0x0100,
        "LB",
        &["leftbumper", "leftshoulder", "l1"],
    ),
    (
        Button::RightBumper,
        0x0200,
        "RB",
        &["rightbumper", "rightshoulder", "r1"],
    ),
    (Button::LeftTrigger, 0x1_0000, "LT", &["lefttrigger", "l2"]),
    (
        Button::RightTrigger,
        0x2_0000,
        "RT",
        &["righttrigger", "r2"],
    ),
    (Button::View, 0x0020, "View", &["back", "select", "share"]),
    (Button::Menu, 0x0010, "Menu", &["start", "options"]),
    (
        Button::LeftStick,
        0x0040,
        "LS",
        &["leftstick", "leftthumb", "l3"],
    ),
    (
        Button::RightStick,
        0x0080,
        "RS",
        &["rightstick", "rightthumb", "r3"],
    ),
    (Button::DPadUp, 0x0001, "DPadUp", &["up", "dup"]),
    (Button::DPadDown, 0x0002, "DPadDown", &["down", "ddown"]),
    (Button::DPadLeft, 0x0004, "DPadLeft", &["left", "dleft"]),
    (Button::DPadRight, 0x0008, "DPadRight", &["right", "dright"]),
    (Button::A, 0x1000, "A", &["cross"]),
    (Button::B, 0x2000, "B", &["circle"]),
    (Button::X, 0x4000, "X", &["square"]),
    (Button::Y, 0x8000, "Y", &["triangle"]),
];

/// Names of the Guide button, which can't be read (see the module docs).
const GUIDE: &[&str] = &["guide", "xbox", "home", "ps"];

impl Button {
    /// The button's bit in a [`Combo`] mask (XInput's flag for buttons).
    pub fn bit(self) -> u32 {
        BUTTONS
            .iter()
            .find(|(b, ..)| *b == self)
            .map_or(0, |(_, bit, ..)| *bit)
    }

    pub fn name(self) -> &'static str {
        BUTTONS
            .iter()
            .find(|(b, ..)| *b == self)
            .map_or("?", |(_, _, name, _)| *name)
    }
}

impl fmt::Display for Button {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Buttons held together, such as `LS+RS`. At least two, so ordinary play
/// doesn't trigger it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Combo(u32);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ComboError {
    #[error("controller combo is empty")]
    Empty,
    #[error(
        "unknown controller button `{0}` (buttons: A, B, X, Y, LB, RB, LT, RT, LS, RS, View, Menu, DPadUp, DPadDown, DPadLeft, DPadRight)"
    )]
    UnknownButton(String),
    #[error("button `{0}` appears twice")]
    DuplicateButton(String),
    #[error("the Xbox (Guide) button can't be read by other apps; pick other buttons")]
    Guide,
    #[error("`{0}` needs at least two buttons so normal play doesn't trigger it")]
    NeedsTwo(String),
}

impl Combo {
    /// Every bit a combo can use.
    pub const ALL: u32 = 0xF3FF | 0x3_0000;

    /// The combo of exactly the buttons in `mask`; `None` if it has bits
    /// that are not buttons, or fewer than two.
    pub fn from_mask(mask: u32) -> Option<Self> {
        (mask & !Self::ALL == 0 && mask.count_ones() >= 2).then_some(Self(mask))
    }

    pub fn mask(self) -> u32 {
        self.0
    }

    /// Number of buttons.
    pub fn len(self) -> u32 {
        self.0.count_ones()
    }

    /// Never true: a combo has at least two buttons.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether all of the combo's buttons are down in `held` (a mask of
    /// the buttons down; others may be down too).
    pub fn is_held(self, held: u32) -> bool {
        held & self.0 == self.0
    }

    /// The buttons, in display order.
    pub fn buttons(self) -> impl Iterator<Item = Button> {
        BUTTONS
            .iter()
            .filter(move |(_, bit, ..)| self.0 & bit != 0)
            .map(|(b, ..)| *b)
    }
}

fn parse_button(token: &str) -> Option<Button> {
    let key: String = token
        .chars()
        .filter(|c| !matches!(c, '-' | '_' | ' '))
        .collect::<String>()
        .to_ascii_lowercase();
    BUTTONS
        .iter()
        .find(|(_, _, name, aliases)| name.eq_ignore_ascii_case(&key) || aliases.contains(&&*key))
        .map(|(b, ..)| *b)
}

impl FromStr for Combo {
    type Err = ComboError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err(ComboError::Empty);
        }
        let mut mask = 0;
        for token in s.split('+').map(str::trim) {
            if GUIDE.contains(&token.to_ascii_lowercase().as_str()) {
                return Err(ComboError::Guide);
            }
            let bit = parse_button(token)
                .ok_or_else(|| ComboError::UnknownButton(token.to_string()))?
                .bit();
            if mask & bit != 0 {
                return Err(ComboError::DuplicateButton(token.to_string()));
            }
            mask |= bit;
        }
        Self::from_mask(mask).ok_or_else(|| ComboError::NeedsTwo(s.to_string()))
    }
}

impl fmt::Display for Combo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, button) in self.buttons().enumerate() {
            if i > 0 {
                f.write_str("+")?;
            }
            f.write_str(button.name())?;
        }
        Ok(())
    }
}

impl TryFrom<String> for Combo {
    type Error = ComboError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Combo> for String {
    fn from(c: Combo) -> Self {
        c.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn combo(s: &str) -> Combo {
        s.parse().unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn parses_and_canonicalises() {
        assert_eq!(combo("rs+ls").to_string(), "LS+RS");
        assert_eq!(combo(" menu + VIEW ").to_string(), "View+Menu");
        assert_eq!(combo("Back+Start").to_string(), "View+Menu");
        assert_eq!(combo("dpad-down + view").to_string(), "View+DPadDown");
        assert_eq!(combo("L1+R1+Triangle").to_string(), "LB+RB+Y");
        assert_eq!(combo("lt+right_trigger").to_string(), "LT+RT");
    }

    #[test]
    fn every_button_round_trips() {
        for &(button, bit, name, aliases) in BUTTONS {
            assert_eq!(button.bit(), bit);
            let other = if button == Button::A { "B" } else { "A" };
            let c = combo(&format!("{name}+{other}"));
            assert!(c.buttons().any(|b| b == button));
            assert_eq!(combo(&c.to_string()), c);
            for alias in aliases {
                assert_eq!(combo(&format!("{alias}+{other}")), c, "{alias}");
            }
        }
        // Distinct bits, all inside ALL.
        let all = BUTTONS.iter().fold(0, |m, (_, bit, ..)| {
            assert_eq!(m & bit, 0);
            m | bit
        });
        assert_eq!(all, Combo::ALL);
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!("".parse::<Combo>(), Err(ComboError::Empty));
        assert_eq!("A".parse::<Combo>(), Err(ComboError::NeedsTwo("A".into())));
        assert_eq!(
            "LS+ls".parse::<Combo>(),
            Err(ComboError::DuplicateButton("ls".into()))
        );
        assert_eq!("Guide+A".parse::<Combo>(), Err(ComboError::Guide));
        assert!(matches!(
            "A+Banana".parse::<Combo>(),
            Err(ComboError::UnknownButton(_))
        ));
        assert!(matches!(
            "A++B".parse::<Combo>(),
            Err(ComboError::UnknownButton(_))
        ));
        assert_eq!(Combo::from_mask(0x1000), None);
        // Guide's bit isn't a combo button.
        assert_eq!(Combo::from_mask(0x1000 | 0x0400), None);
    }

    #[test]
    fn held_means_all_buttons_down() {
        let c = combo("LS+RS");
        assert!(c.is_held(0x40 | 0x80));
        // Other buttons may be down too.
        assert!(c.is_held(0x40 | 0x80 | 0x1000));
        assert!(!c.is_held(0x40));
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn serde_uses_strings() {
        let json = serde_json::to_string(&combo("start+back")).unwrap();
        assert_eq!(json, "\"View+Menu\"");
        assert!(serde_json::from_str::<Combo>("\"A\"").is_err());
    }
}
