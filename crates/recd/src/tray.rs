//! Notification-area icon. Its colour always tells the truth about whether
//! rec is recording, so the buffer can never be "silently off".

use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject, HBITMAP,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos,
    GetSystemMetrics, HICON, HMENU, ICONINFO, MF_DISABLED, MF_GRAYED, MF_SEPARATOR, MF_STRING,
    SM_CXSMICON, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TrackPopupMenu,
};
use windows::core::PCWSTR;

use crate::win::{Hwnd, wide};

pub const WM_TRAY: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    Recording,
    Idle,
    Paused,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuCommand {
    SaveClip = 1,
    TogglePause,
    QuickPanel,
    OpenClips,
    OpenConfig,
    OpenLog,
    Quit,
}

impl MenuCommand {
    fn from_id(id: u32) -> Option<Self> {
        [
            Self::SaveClip,
            Self::TogglePause,
            Self::QuickPanel,
            Self::OpenClips,
            Self::OpenConfig,
            Self::OpenLog,
            Self::Quit,
        ]
        .into_iter()
        .find(|c| *c as u32 == id)
    }
}

#[derive(Debug)]
pub struct Tray {
    hwnd: Hwnd,
    icons: [HICON; 4],
    added: bool,
}

/// Draws an anti-aliased dot (or ring) icon with per-pixel alpha.
fn make_icon(size: i32, rgb: (u8, u8, u8), ring_only: bool) -> HICON {
    let header = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: size,
        biHeight: -size,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let info = BITMAPINFO {
        bmiHeader: header,
        ..Default::default()
    };
    // SAFETY: the DIB section's bits pointer is valid for size*size*4 bytes
    // until the bitmap is deleted, which happens after CreateIconIndirect has
    // copied it.
    unsafe {
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let Ok(color): Result<HBITMAP, _> =
            CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)
        else {
            return HICON::default();
        };
        let pixels = std::slice::from_raw_parts_mut(bits.cast::<u32>(), (size * size) as usize);
        let center = size as f32 / 2.0;
        let outer = size as f32 * 0.42;
        let inner = outer - (size as f32 * 0.16).max(2.0);
        for y in 0..size {
            for x in 0..size {
                let dx = x as f32 + 0.5 - center;
                let dy = y as f32 + 0.5 - center;
                let d = (dx * dx + dy * dy).sqrt();
                let mut a = (outer - d + 0.5).clamp(0.0, 1.0);
                if ring_only {
                    a *= (d - inner + 0.5).clamp(0.0, 1.0);
                }
                let alpha = (a * 255.0) as u32;
                // Premultiplied BGRA.
                let premul = |c: u8| u32::from(c) * alpha / 255;
                pixels[(y * size + x) as usize] =
                    (alpha << 24) | (premul(rgb.0) << 16) | (premul(rgb.1) << 8) | premul(rgb.2);
            }
        }
        let mask = CreateBitmap(size, size, 1, 1, None);
        let icon_info = ICONINFO {
            fIcon: true.into(),
            hbmMask: mask,
            hbmColor: color,
            ..Default::default()
        };
        let icon = CreateIconIndirect(&icon_info).unwrap_or_default();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

fn copy_wide(dst: &mut [u16], text: &str) {
    let src: Vec<u16> = text.encode_utf16().take(dst.len() - 1).collect();
    dst[..src.len()].copy_from_slice(&src);
    dst[src.len()] = 0;
}

impl Tray {
    pub fn new(hwnd: Hwnd) -> Self {
        // SAFETY: GetSystemMetrics has no preconditions.
        let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
        Self {
            hwnd,
            icons: [
                make_icon(size, (0xE5, 0x39, 0x35), false), // recording: red dot
                make_icon(size, (0x9E, 0x9E, 0x9E), true),  // idle: grey ring
                make_icon(size, (0xFF, 0xB3, 0x00), false), // paused: amber dot
                make_icon(size, (0xFF, 0x57, 0x22), true),  // error: orange ring
            ],
            added: false,
        }
    }

    fn data(&self, look: Look, tip: &str) -> NOTIFYICONDATAW {
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd.get(),
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP,
            uCallbackMessage: WM_TRAY,
            hIcon: self.icons[look as usize],
            ..Default::default()
        };
        copy_wide(&mut data.szTip, tip);
        data
    }

    /// Shows the icon, or updates it. Also re-adds it after Explorer restarts.
    pub fn show(&mut self, look: Look, tip: &str) {
        let mut data = self.data(look, tip);
        // SAFETY: `data` is fully initialised and outlives the calls.
        unsafe {
            if self.added && Shell_NotifyIconW(NIM_MODIFY, &data).as_bool() {
                return;
            }
            self.added = Shell_NotifyIconW(NIM_ADD, &data).as_bool();
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
        }
    }

    /// Call on `TaskbarCreated`: Explorer restarted and forgot our icon.
    pub fn reset(&mut self) {
        self.added = false;
    }

    /// Shows the context menu at the cursor and returns the chosen command.
    /// `panel_hotkey` is shown next to the quick panel entry.
    pub fn menu(&self, status: &str, paused: bool, panel_hotkey: &str) -> Option<MenuCommand> {
        // SAFETY: the menu is created, used modally and destroyed here; all
        // strings outlive the calls that read them.
        unsafe {
            let menu: HMENU = CreatePopupMenu().ok()?;
            let add = |flags, id: Option<MenuCommand>, text: &str| {
                let text = wide(text);
                let _ = AppendMenuW(
                    menu,
                    flags,
                    id.map_or(0, |c| c as usize),
                    PCWSTR(text.as_ptr()),
                );
            };
            add(MF_STRING | MF_DISABLED | MF_GRAYED, None, status);
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            add(MF_STRING, Some(MenuCommand::SaveClip), "Save clip now");
            add(
                MF_STRING,
                Some(MenuCommand::TogglePause),
                if paused {
                    "Resume recording"
                } else {
                    "Pause recording"
                },
            );
            add(
                MF_STRING,
                Some(MenuCommand::QuickPanel),
                &format!("Quick panel\t{panel_hotkey}"),
            );
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            add(MF_STRING, Some(MenuCommand::OpenClips), "Open clips folder");
            add(
                MF_STRING,
                Some(MenuCommand::OpenConfig),
                "Edit settings file",
            );
            add(MF_STRING, Some(MenuCommand::OpenLog), "Open log");
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            add(MF_STRING, Some(MenuCommand::Quit), "Quit rec");

            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            // Required so the menu closes when the user clicks elsewhere.
            let _ = SetForegroundWindow(self.hwnd.get());
            let id = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
                pt.x,
                pt.y,
                None,
                self.hwnd.get(),
                None,
            );
            let _ = DestroyMenu(menu);
            MenuCommand::from_id(id.0 as u32)
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        let data = self.data(Look::Idle, "");
        // SAFETY: removing our own icon and destroying icons we created.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
            for icon in self.icons {
                let _ = DestroyIcon(icon);
            }
        }
    }
}
