//! Small Win32 helpers shared by the modules.

use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::HWND;
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows::core::{PCWSTR, w};

/// NUL-terminated UTF-16 for Win32 calls.
pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

/// A window handle that can cross threads. Windows handles are plain
/// integers to the OS; only the raw-pointer wrapper in `windows` is `!Send`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Hwnd(pub isize);

impl Hwnd {
    pub fn get(self) -> HWND {
        HWND(self.0 as *mut c_void)
    }
}

impl From<HWND> for Hwnd {
    fn from(hwnd: HWND) -> Self {
        Self(hwnd.0 as isize)
    }
}

/// Reads a string (e.g. `ProductName`) from an executable's version resource.
pub fn version_string(path: &Path, key: &str) -> Option<String> {
    let wpath = wide(path);
    // SAFETY: `data` is sized by GetFileVersionInfoSizeW and outlives every
    // pointer VerQueryValueW returns into it; lengths come from the API.
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(wpath.as_ptr()), None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(PCWSTR(wpath.as_ptr()), None, size, data.as_mut_ptr().cast()).ok()?;

        let mut ptr: *mut c_void = std::ptr::null_mut();
        let mut len = 0u32;
        if !VerQueryValueW(
            data.as_ptr().cast(),
            w!("\\VarFileInfo\\Translation"),
            &mut ptr,
            &mut len,
        )
        .as_bool()
            || len < 4
        {
            return None;
        }
        let lang = *(ptr as *const u16);
        let codepage = *(ptr as *const u16).add(1);

        let sub = wide(format!("\\StringFileInfo\\{lang:04x}{codepage:04x}\\{key}"));
        if !VerQueryValueW(
            data.as_ptr().cast(),
            PCWSTR(sub.as_ptr()),
            &mut ptr,
            &mut len,
        )
        .as_bool()
            || len == 0
        {
            return None;
        }
        let chars = std::slice::from_raw_parts(ptr as *const u16, len as usize);
        let end = chars.iter().position(|&c| c == 0).unwrap_or(chars.len());
        Some(String::from_utf16_lossy(&chars[..end]))
    }
}
