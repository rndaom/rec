//! Global hotkeys via `RegisterHotKey`: no keyboard hook, no added input
//! latency, and it works whichever window has focus.

use rec_core::config::HotkeyConfig;
use rec_core::hotkey::{Hotkey, Key, NamedKey};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
    UnregisterHotKey,
};

use crate::log::warning;
use crate::win::Hwnd;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    SaveClip,
    /// Save with a specific length.
    SaveSeconds(u32),
    ToggleOverlay,
    AddMarker,
    TogglePause,
}

#[derive(Debug, Default)]
pub struct Hotkeys {
    hwnd: Hwnd,
    registered: Vec<(i32, Action)>,
    /// Human-readable registration failures, shown in the tray and status.
    pub failures: Vec<String>,
}

fn virtual_key(key: Key) -> u32 {
    match key {
        Key::Letter(c) => c as u32,
        Key::Digit(d) => u32::from(b'0' + d),
        Key::Function(n) => 0x70 + u32::from(n) - 1,
        Key::Numpad(n) => 0x60 + u32::from(n),
        Key::Named(named) => match named {
            NamedKey::Space => 0x20,
            NamedKey::Tab => 0x09,
            NamedKey::Enter => 0x0D,
            NamedKey::Escape => 0x1B,
            NamedKey::Backspace => 0x08,
            NamedKey::Insert => 0x2D,
            NamedKey::Delete => 0x2E,
            NamedKey::Home => 0x24,
            NamedKey::End => 0x23,
            NamedKey::PageUp => 0x21,
            NamedKey::PageDown => 0x22,
            NamedKey::Up => 0x26,
            NamedKey::Down => 0x28,
            NamedKey::Left => 0x25,
            NamedKey::Right => 0x27,
            NamedKey::PrintScreen => 0x2C,
            NamedKey::ScrollLock => 0x91,
            NamedKey::Pause => 0x13,
        },
    }
}

fn modifiers(hotkey: &Hotkey) -> HOT_KEY_MODIFIERS {
    let m = hotkey.modifiers;
    let mut flags = MOD_NOREPEAT;
    if m.ctrl {
        flags |= MOD_CONTROL;
    }
    if m.alt {
        flags |= MOD_ALT;
    }
    if m.shift {
        flags |= MOD_SHIFT;
    }
    if m.win {
        flags |= MOD_WIN;
    }
    flags
}

impl Hotkeys {
    /// Registers every configured hotkey on `hwnd`, replacing earlier ones.
    pub fn register(&mut self, hwnd: Hwnd, config: &HotkeyConfig) {
        self.unregister();
        self.hwnd = hwnd;

        let mut bindings = vec![
            (config.save_clip, Action::SaveClip),
            (config.toggle_overlay, Action::ToggleOverlay),
        ];
        bindings.extend(config.add_marker.map(|h| (h, Action::AddMarker)));
        bindings.extend(config.pause.map(|h| (h, Action::TogglePause)));
        bindings.extend(
            config
                .extra_clips
                .iter()
                .map(|c| (c.hotkey, Action::SaveSeconds(c.seconds))),
        );

        for (id, (hotkey, action)) in (1..).zip(bindings) {
            // SAFETY: `hwnd` is our live message window on this thread.
            let result = unsafe {
                RegisterHotKey(
                    Some(hwnd.get()),
                    id,
                    modifiers(&hotkey),
                    virtual_key(hotkey.key),
                )
            };
            match result {
                Ok(()) => self.registered.push((id, action)),
                Err(e) => {
                    let owner = hotkey
                        .known_conflict()
                        .map_or(String::new(), |o| format!(" (used by {o})"));
                    let message = format!("{hotkey} is taken by another app{owner}");
                    warning!("hotkey {hotkey} for {action:?} failed: {e}");
                    self.failures.push(message);
                }
            }
        }
    }

    pub fn unregister(&mut self) {
        for (id, _) in self.registered.drain(..) {
            // SAFETY: unregistering an id we registered on the same window.
            let _ = unsafe { UnregisterHotKey(Some(self.hwnd.get()), id) };
        }
        self.failures.clear();
    }

    pub fn action(&self, id: i32) -> Option<Action> {
        self.registered
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, a)| *a)
    }
}

impl Drop for Hotkeys {
    fn drop(&mut self) {
        self.unregister();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_virtual_keys() {
        let vk = |s: &str| virtual_key(s.parse::<Hotkey>().unwrap().key);
        assert_eq!(vk("Alt+F8"), 0x77);
        assert_eq!(vk("Ctrl+S"), u32::from(b'S'));
        assert_eq!(vk("Ctrl+7"), u32::from(b'7'));
        assert_eq!(vk("Ctrl+Num3"), 0x63);
        assert_eq!(vk("F24"), 0x87);
        assert_eq!(vk("Alt+PageDown"), 0x22);
    }
}
