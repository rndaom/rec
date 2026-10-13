//! "Clip saved" feedback that never steals focus from the game: a short
//! synthesized sound and a small toast window that is click-through,
//! non-activating and excluded from screen capture (so it never appears in
//! clips). Windows notifications are not used because Focus Assist hides
//! them while gaming.

use std::sync::OnceLock;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER,
    BLENDFUNCTION, CLIP_DEFAULT_PRECIS, CreateCompatibleDC, CreateDIBSection, CreateFontW,
    DEFAULT_CHARSET, DIB_RGB_COLORS, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_TOP,
    DeleteDC, DeleteObject, DrawTextW, FF_DONTCARE, FW_NORMAL, FW_SEMIBOLD, GetDC, GetMonitorInfoW,
    HMONITOR, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint, OUT_DEFAULT_PRECIS,
    ReleaseDC, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, HWND_TOPMOST, MA_NOACTIVATE, RegisterClassW, SW_HIDE,
    SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowDisplayAffinity,
    SetWindowPos, ShowWindow, ULW_ALPHA, UpdateLayeredWindow, WDA_EXCLUDEFROMCAPTURE,
    WM_MOUSEACTIVATE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use crate::win::{Hwnd, wide};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Success,
    Info,
    Error,
}

// ---- sound -----------------------------------------------------------------

const SAMPLE_RATE: u32 = 44_100;

/// A tiny WAV (16-bit mono PCM) made of soft sine notes `(Hz, ms)`.
fn synth(notes: &[(f32, f32)]) -> Vec<u8> {
    let mut samples = Vec::new();
    for &(freq, ms) in notes {
        let n = (SAMPLE_RATE as f32 * ms / 1000.0) as usize;
        for i in 0..n {
            let t = i as f32 / SAMPLE_RATE as f32;
            let attack = (t / 0.004).min(1.0);
            let decay = (-(t * 1000.0 / ms) * 4.0).exp();
            let v = (t * freq * std::f32::consts::TAU).sin() * attack * decay * 0.22;
            samples.push((v * i16::MAX as f32) as i16);
        }
    }
    let data_len = (samples.len() * 2) as u32;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        wav.extend_from_slice(&s.to_le_bytes());
    }
    wav
}

pub fn play(kind: Kind) {
    static SUCCESS: OnceLock<Vec<u8>> = OnceLock::new();
    static ERROR: OnceLock<Vec<u8>> = OnceLock::new();
    let wav = match kind {
        Kind::Success | Kind::Info => {
            SUCCESS.get_or_init(|| synth(&[(880.0, 70.0), (1318.5, 110.0)]))
        }
        Kind::Error => ERROR.get_or_init(|| synth(&[(392.0, 120.0), (261.6, 180.0)])),
    };
    // SAFETY: SND_MEMORY reads the WAV from the pointer; the buffer is a
    // static, so it outlives the asynchronous playback.
    unsafe {
        let _ = PlaySoundW(
            PCWSTR(wav.as_ptr().cast()),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        );
    }
}

// ---- toast -----------------------------------------------------------------

#[derive(Debug)]
pub struct Toast {
    hwnd: Hwnd,
}

unsafe extern "system" fn toast_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE as isize);
    }
    // SAFETY: forwarding the original arguments.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Premultiplied BGRA.
fn pixel(rgb: u32, alpha: u32) -> u32 {
    let premul = |shift: u32| ((rgb >> shift) & 0xFF) * alpha / 255;
    (alpha << 24) | (premul(16) << 16) | (premul(8) << 8) | premul(0)
}

impl Toast {
    pub fn new() -> windows::core::Result<Self> {
        // SAFETY: registering a class with a static name and a valid window
        // procedure, then creating a hidden popup of that class.
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(toast_proc),
                hInstance: instance.into(),
                lpszClassName: w!("rec.toast"),
                ..Default::default()
            };
            RegisterClassW(&class);
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE,
                w!("rec.toast"),
                w!("rec"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance.into()),
                None,
            )?;
            // Keep our own UI out of every capture, including our clips.
            // REC_DEBUG_CAPTURE_UI=1 makes it visible to screenshots for testing.
            if std::env::var_os("REC_DEBUG_CAPTURE_UI").is_none() {
                let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
            }
            Ok(Self { hwnd: hwnd.into() })
        }
    }

    /// Shows the toast in the top-right corner of `monitor` (primary if
    /// `None`). The caller hides it with [`Toast::hide`] after a timeout.
    pub fn show(&self, kind: Kind, title: &str, detail: &str, monitor: Option<HMONITOR>) {
        // SAFETY: every GDI object created here is selected out and deleted
        // before returning; the DIB pixel slice is only used while the
        // bitmap is alive.
        unsafe {
            let monitor = monitor
                .unwrap_or_else(|| MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY));
            let mut mi = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(monitor, &mut mi).as_bool() {
                return;
            }
            let (mut dpi_x, mut dpi_y) = (96, 96);
            let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
            let scale = |v: i32| v * dpi_x as i32 / 96;

            let (width, height) = (scale(320), scale(64));
            let margin = scale(24);
            let pos = POINT {
                x: mi.rcWork.right - width - margin,
                y: mi.rcWork.top + margin,
            };

            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            let mut bits = std::ptr::null_mut();
            let Ok(bitmap) = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            else {
                let _ = DeleteDC(dc);
                ReleaseDC(None, screen);
                return;
            };
            let old_bitmap = SelectObject(dc, bitmap.into());
            let pixels =
                std::slice::from_raw_parts_mut(bits.cast::<u32>(), (width * height) as usize);

            // Background: rounded dark card with an accent bar on the left.
            let radius = scale(10) as f32;
            let accent = match kind {
                Kind::Success => 0x4C_AF50,
                Kind::Info => 0x42_A5F5,
                Kind::Error => 0xFF_5722,
            };
            let bar = scale(5);
            for y in 0..height {
                for x in 0..width {
                    let fx = x as f32 + 0.5;
                    let fy = y as f32 + 0.5;
                    let cx = fx.clamp(radius, width as f32 - radius);
                    let cy = fy.clamp(radius, height as f32 - radius);
                    let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
                    let coverage = (radius - d + 0.5).clamp(0.0, 1.0);
                    let rgb = if x < bar { accent } else { 0x1F_1F1F };
                    pixels[(y * width + x) as usize] = pixel(rgb, (coverage * 240.0) as u32);
                }
            }

            // Text. GDI writes alpha = 0 into every pixel it touches; we fix
            // that up below.
            SetBkMode(dc, TRANSPARENT);
            let font = |size: i32, weight| {
                CreateFontW(
                    -scale(size),
                    0,
                    0,
                    0,
                    weight as i32,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET,
                    OUT_DEFAULT_PRECIS,
                    CLIP_DEFAULT_PRECIS,
                    ANTIALIASED_QUALITY,
                    FF_DONTCARE.0 as u32,
                    w!("Segoe UI"),
                )
            };
            let title_font = font(15, FW_SEMIBOLD.0);
            let detail_font = font(13, FW_NORMAL.0);
            let flags = DT_LEFT | DT_TOP | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX;

            let old_font = SelectObject(dc, title_font.into());
            SetTextColor(dc, COLORREF(0x00FF_FFFF));
            let mut title_w = wide(title);
            title_w.pop();
            let mut rect = RECT {
                left: bar + scale(14),
                top: scale(12),
                right: width - scale(14),
                bottom: scale(34),
            };
            DrawTextW(dc, &mut title_w, &mut rect, flags);

            SelectObject(dc, detail_font.into());
            SetTextColor(dc, COLORREF(0x00BD_BDBD));
            let mut detail_w = wide(detail);
            detail_w.pop();
            rect.top = scale(34);
            rect.bottom = height - scale(8);
            DrawTextW(dc, &mut detail_w, &mut rect, flags);

            SelectObject(dc, old_font);
            let _ = DeleteObject(title_font.into());
            let _ = DeleteObject(detail_font.into());

            // Pixels inside the card whose alpha GDI zeroed are text: make
            // them opaque (their colour is already blended onto the card).
            let inset = radius as i32;
            for y in 0..height {
                for x in inset..width - inset {
                    let p = &mut pixels[(y * width + x) as usize];
                    if *p >> 24 == 0 && y > 0 && y < height - 1 {
                        *p |= 0xFF00_0000;
                    }
                }
            }

            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let size = SIZE {
                cx: width,
                cy: height,
            };
            let src = POINT::default();
            let _ = UpdateLayeredWindow(
                self.hwnd.get(),
                Some(screen),
                Some(&pos),
                Some(&size),
                Some(dc),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );

            SelectObject(dc, old_bitmap);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);

            let _ = ShowWindow(self.hwnd.get(), SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(
                self.hwnd.get(),
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }

    pub fn hide(&self) {
        // SAFETY: hiding our own window.
        unsafe {
            let _ = ShowWindow(self.hwnd.get(), SW_HIDE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesized_wav_is_well_formed() {
        let wav = synth(&[(440.0, 100.0)]);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        let data_len = u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize;
        assert_eq!(wav.len(), 44 + data_len);
        assert_eq!(data_len, 2 * (SAMPLE_RATE as usize / 10));
    }

    #[test]
    fn premultiplies() {
        assert_eq!(pixel(0xFF_FFFF, 255), 0xFFFF_FFFF);
        assert_eq!(pixel(0xFF_0000, 0), 0);
        assert_eq!(pixel(0xFF_0000, 128) >> 16 & 0xFF, 128);
    }
}
