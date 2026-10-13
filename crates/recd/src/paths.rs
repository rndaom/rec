//! Where rec keeps its files, and how clips are named.

use std::path::PathBuf;

use rec_core::config::{self, Config};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{FOLDERID_Videos, KF_FLAG_DEFAULT, SHGetKnownFolderPath};

/// `%LOCALAPPDATA%\rec`: logs and other machine-local state.
pub fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
        .join("rec")
}

/// The user's Videos known folder, which may have been relocated.
fn videos_dir() -> Option<PathBuf> {
    // SAFETY: on success the returned buffer is a valid NUL-terminated string
    // that we own and free with CoTaskMemFree after copying it.
    unsafe {
        let raw = SHGetKnownFolderPath(&FOLDERID_Videos, KF_FLAG_DEFAULT, None).ok()?;
        let path = raw.to_string().ok();
        CoTaskMemFree(Some(raw.0 as *const _));
        path.map(PathBuf::from)
    }
}

/// Root folder for saved clips.
pub fn clips_dir(config: &Config) -> PathBuf {
    if let Some(dir) = &config.output.directory {
        return dir.clone();
    }
    videos_dir().map_or_else(config::fallback_output_dir, |videos| videos.join("rec"))
}

/// Local date and time, already split into the parts filenames use.
#[derive(Clone, Copy, Debug)]
pub struct Stamp {
    pub year: u16,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
}

impl Stamp {
    pub fn now() -> Self {
        // SAFETY: GetLocalTime has no preconditions.
        let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
        Self {
            year: t.wYear,
            month: t.wMonth,
            day: t.wDay,
            hour: t.wHour,
            minute: t.wMinute,
            second: t.wSecond,
        }
    }
}

/// Fills `{game}`, `{date}`, `{time}` and `{seconds}` and makes the result a
/// valid Windows file name (without extension).
pub fn render_filename(template: &str, game: &str, stamp: Stamp, seconds: u32) -> String {
    let date = format!("{:04}-{:02}-{:02}", stamp.year, stamp.month, stamp.day);
    let time = format!("{:02}-{:02}-{:02}", stamp.hour, stamp.minute, stamp.second);
    let name = template
        .replace("{game}", game)
        .replace("{date}", &date)
        .replace("{time}", &time)
        .replace("{seconds}", &seconds.to_string());
    let name = sanitize(&name);
    if name.is_empty() {
        format!("clip_{date}_{time}")
    } else {
        name
    }
}

/// Replaces characters Windows forbids in file names, trims trailing dots
/// and spaces, avoids reserved device names and caps the length.
pub fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .take(120)
        .collect();
    let cleaned = cleaned.trim().trim_end_matches(['.', ' ']).to_string();
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(&cleaned)) {
        format!("_{cleaned}")
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAMP: Stamp = Stamp {
        year: 2026,
        month: 3,
        day: 7,
        hour: 9,
        minute: 5,
        second: 2,
    };

    #[test]
    fn renders_template() {
        assert_eq!(
            render_filename("{game}_{date}_{time}", "Counter-Strike 2", STAMP, 60),
            "Counter-Strike 2_2026-03-07_09-05-02"
        );
        assert_eq!(
            render_filename("{game} {seconds}s", "x", STAMP, 30),
            "x 30s"
        );
    }

    #[test]
    fn sanitizes_file_names() {
        assert_eq!(sanitize("a<b>c:d\"e/f\\g|h?i*j"), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(sanitize("name. . "), "name");
        assert_eq!(sanitize("con"), "_con");
        assert_eq!(sanitize(&"x".repeat(500)).len(), 120);
        assert_eq!(
            render_filename("{game}", "...", STAMP, 1),
            "clip_2026-03-07_09-05-02"
        );
    }
}
