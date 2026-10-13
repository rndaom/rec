//! Minimal append-only log at `%LOCALAPPDATA%\rec\recd.log`.
//!
//! Lines are short and rare (state changes, saves, errors), so a mutex around
//! a file is plenty. The file is rotated to `recd.old.log` past 1 MB at start.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;

use windows::Win32::System::SystemInformation::GetLocalTime;

const MAX_LOG_BYTES: u64 = 1024 * 1024;

static LOG: Mutex<Option<File>> = Mutex::new(None);

pub fn init() {
    let dir = crate::paths::data_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("recd.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let _ = std::fs::rename(&path, dir.join("recd.old.log"));
    }
    if let Ok(file) = OpenOptions::new().create(true).append(true).open(path) {
        *LOG.lock().unwrap_or_else(|e| e.into_inner()) = Some(file);
    }
}

pub fn write(level: &str, args: fmt::Arguments<'_>) {
    // SAFETY: GetLocalTime has no preconditions.
    let t = unsafe { GetLocalTime() };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} {level:5} {args}\n",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    );
    if cfg!(debug_assertions) {
        eprint!("{line}");
    }
    if let Some(file) = LOG.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        let _ = file.write_all(line.as_bytes());
    }
}

macro_rules! info {
    ($($arg:tt)*) => { $crate::log::write("INFO", format_args!($($arg)*)) };
}
macro_rules! warning {
    ($($arg:tt)*) => { $crate::log::write("WARN", format_args!($($arg)*)) };
}
macro_rules! error {
    ($($arg:tt)*) => { $crate::log::write("ERROR", format_args!($($arg)*)) };
}
pub(crate) use {error, info, warning};
