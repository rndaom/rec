//! Decides whether the foreground window is a game.
//!
//! Only out-of-process information is used: window geometry, class names and
//! the executable path (read with `PROCESS_QUERY_LIMITED_INFORMATION`, the
//! weakest process right, then closed immediately). Nothing touches the game
//! process itself, which keeps anti-cheat happy.

use std::path::{Path, PathBuf};

use rec_core::config::BehaviorConfig;
use windows::Win32::Foundation::{CloseHandle, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GWL_STYLE, GetAncestor, GetClassNameW, GetWindowLongW, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, IsZoomed, WS_CAPTION,
};
use windows::core::PWSTR;

/// Facts about a top-level window, gathered once per foreground change.
#[derive(Clone, Debug)]
pub struct WindowInfo {
    pub hwnd: isize,
    pub pid: u32,
    pub exe_path: PathBuf,
    pub exe_name: String,
    pub title: String,
    pub class: String,
    /// The window covers its whole monitor (exclusive or borderless fullscreen).
    pub covers_monitor: bool,
    pub visible: bool,
}

/// Apps that often run fullscreen but are not games.
const NOT_GAMES: &[&str] = &[
    "explorer.exe",
    "ApplicationFrameHost.exe",
    "chrome.exe",
    "msedge.exe",
    "firefox.exe",
    "brave.exe",
    "opera.exe",
    "vivaldi.exe",
    "vlc.exe",
    "mpc-hc64.exe",
    "mpc-be64.exe",
    "PotPlayerMini64.exe",
    "Microsoft.Media.Player.exe",
    "Video.UI.exe",
    "POWERPNT.EXE",
    "mstsc.exe",
    "obs64.exe",
    "Discord.exe",
    "Spotify.exe",
    "Teams.exe",
    "ms-teams.exe",
    "Zoom.exe",
    "WindowsTerminal.exe",
    "Code.exe",
    "devenv.exe",
    "steam.exe",
    "steamwebhelper.exe",
    "EpicGamesLauncher.exe",
    "Battle.net.exe",
    "RiotClientServices.exe",
    "RiotClientUx.exe",
    "EADesktop.exe",
    "upc.exe",
    "GalaxyClient.exe",
    "XboxPcApp.exe",
    "recd.exe",
    "rec.exe",
    "rec-ui.exe",
];

/// Shell surfaces that can be "foreground" but are never games.
const SHELL_CLASSES: &[&str] = &[
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
];

/// Install folders of game stores; anything running from here is a game,
/// windowed or not.
const GAME_LIBRARY_DIRS: &[&str] = &[
    r"\steamapps\common\",
    r"\epic games\",
    r"\xboxgames\",
    r"\gog galaxy\games\",
    r"\riot games\",
    r"\ea games\",
    r"\ubisoft game launcher\games\",
];

/// Games that run in a shared runtime, so neither the exe nor its folder
/// says "game": (runtime exes, window title prefix, game name). Minecraft:
/// Java Edition plays windowed in `javaw.exe` (titles "Minecraft 1.6.4",
/// "Minecraft* 1.21.1 - Multiplayer"); the old Java launcher has the same exe
/// and is excluded by title.
const HOSTED_GAMES: &[(&[&str], &str, &str)] =
    &[(&["javaw.exe", "java.exe"], "Minecraft", "Minecraft")];
const HOSTED_EXCLUDED_TITLES: &[&str] = &["Minecraft Launcher"];

/// Games outside every store library folder that often play windowed.
const KNOWN_GAMES: &[&str] = &["Minecraft.Windows.exe"];

/// The game a shared runtime's window is running, by its title.
fn hosted_game(exe_name: &str, title: &str) -> Option<&'static str> {
    if HOSTED_EXCLUDED_TITLES.iter().any(|t| title.starts_with(t)) {
        return None;
    }
    HOSTED_GAMES
        .iter()
        .find(|(exes, prefix, _)| listed(exes, exe_name) && title.starts_with(prefix))
        .map(|&(_, _, name)| name)
}

fn listed(list: &[impl AsRef<str>], exe_name: &str) -> bool {
    list.iter()
        .any(|e| e.as_ref().eq_ignore_ascii_case(exe_name))
}

pub fn is_private(info: &WindowInfo, behavior: &BehaviorConfig) -> bool {
    listed(&behavior.private_apps, &info.exe_name)
}

/// The classification rules, kept free of Win32 calls so they are testable.
pub fn is_game(info: &WindowInfo, behavior: &BehaviorConfig, own_pid: u32) -> bool {
    if info.pid == own_pid || !info.visible || SHELL_CLASSES.contains(&info.class.as_str()) {
        return false;
    }
    if listed(&behavior.ignored_apps, &info.exe_name) || is_private(info, behavior) {
        return false;
    }
    if listed(&behavior.game_apps, &info.exe_name)
        || listed(KNOWN_GAMES, &info.exe_name)
        || hosted_game(&info.exe_name, &info.title).is_some()
    {
        return true;
    }
    if listed(NOT_GAMES, &info.exe_name) {
        return false;
    }
    let path = info.exe_path.to_string_lossy().to_ascii_lowercase();
    GAME_LIBRARY_DIRS.iter().any(|dir| path.contains(dir)) || info.covers_monitor
}

pub fn own_pid() -> u32 {
    // SAFETY: no preconditions.
    unsafe { GetCurrentProcessId() }
}

fn exe_path(pid: u32) -> Option<PathBuf> {
    // SAFETY: the handle is checked, used for one query and closed; the
    // buffer length is passed in and updated by the call.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(process);
        result.ok()?;
        Some(PathBuf::from(String::from_utf16_lossy(
            &buf[..len as usize],
        )))
    }
}

fn contains(outer: &RECT, inner: &RECT) -> bool {
    outer.left <= inner.left
        && outer.top <= inner.top
        && outer.right >= inner.right
        && outer.bottom >= inner.bottom
}

/// Gathers [`WindowInfo`] for the top-level window owning `hwnd`.
pub fn window_info(hwnd: HWND) -> Option<WindowInfo> {
    if hwnd.is_invalid() {
        return None;
    }
    // SAFETY: all calls take a window handle that may be stale; they fail
    // gracefully on invalid handles. Buffers are sized locals.
    unsafe {
        let root = GetAncestor(hwnd, GA_ROOT);
        let hwnd = if root.is_invalid() { hwnd } else { root };

        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }

        let mut class = [0u16; 128];
        let class_len = GetClassNameW(hwnd, &mut class).max(0) as usize;
        let mut title = [0u16; 256];
        let title_len = GetWindowTextW(hwnd, &mut title).max(0) as usize;

        // A maximized window with a title bar spills past the work area and
        // can cover the monitor when the taskbar auto-hides; it isn't fullscreen.
        let captioned = GetWindowLongW(hwnd, GWL_STYLE) as u32 & WS_CAPTION.0 == WS_CAPTION.0;
        let maximized_app = captioned && IsZoomed(hwnd).as_bool();

        let mut rect = RECT::default();
        let mut covers_monitor = false;
        if !maximized_app && GetWindowRect(hwnd, &mut rect).is_ok() {
            let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(monitor, &mut mi).as_bool() {
                covers_monitor = contains(&rect, &mi.rcMonitor);
            }
        }

        let exe_path = exe_path(pid).unwrap_or_default();
        let exe_name = exe_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Some(WindowInfo {
            hwnd: hwnd.0 as isize,
            pid,
            exe_path,
            exe_name,
            title: String::from_utf16_lossy(&title[..title_len]),
            class: String::from_utf16_lossy(&class[..class_len]),
            covers_monitor,
            visible: IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool(),
        })
    }
}

/// A friendly game name for folders and file names: the game a shared
/// runtime runs (see `HOSTED_GAMES`), else the executable's product name or
/// description, else the window title up to the first " - " (titles like
/// "Team Fortress 2 - Direct3D 9 - 64 Bit"), else the file stem.
pub fn display_name(exe_path: &Path, title: &str) -> String {
    let exe_name = exe_path.file_name().map(|n| n.to_string_lossy());
    if let Some(game) = exe_name.and_then(|n| hosted_game(&n, title)) {
        return game.to_string();
    }
    let usable = |name: &str| !name.trim().is_empty() && name.trim().len() <= 60;
    crate::win::version_string(exe_path, "ProductName")
        .or_else(|| crate::win::version_string(exe_path, "FileDescription"))
        .filter(|name| usable(name))
        .map(|name| name.trim().to_string())
        .or_else(|| title_name(title).filter(|name| usable(name)))
        .unwrap_or_else(|| {
            exe_path.file_stem().map_or_else(
                || "Unknown".to_string(),
                |s| s.to_string_lossy().into_owned(),
            )
        })
}

fn title_name(title: &str) -> Option<String> {
    let name = title.split(" - ").next()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(exe: &str, path: &str, covers_monitor: bool) -> WindowInfo {
        WindowInfo {
            hwnd: 1,
            pid: 42,
            exe_path: PathBuf::from(path),
            exe_name: exe.into(),
            title: String::new(),
            class: "SomeClass".into(),
            covers_monitor,
            visible: true,
        }
    }

    #[test]
    fn fullscreen_unknown_app_is_a_game() {
        let b = BehaviorConfig::default();
        assert!(is_game(
            &window("game.exe", r"D:\Games\game.exe", true),
            &b,
            1
        ));
        assert!(!is_game(
            &window("game.exe", r"D:\Games\game.exe", false),
            &b,
            1
        ));
    }

    #[test]
    fn store_library_counts_even_windowed() {
        let b = BehaviorConfig::default();
        let w = window(
            "cs2.exe",
            r"C:\Steam\steamapps\common\Counter-Strike Global Offensive\cs2.exe",
            false,
        );
        assert!(is_game(&w, &b, 1));
    }

    #[test]
    fn fullscreen_browsers_and_launchers_are_not_games() {
        let b = BehaviorConfig::default();
        assert!(!is_game(
            &window("chrome.exe", r"C:\x\chrome.exe", true),
            &b,
            1
        ));
        assert!(!is_game(
            &window("STEAM.EXE", r"C:\Steam\steam.exe", true),
            &b,
            1
        ));
    }

    #[test]
    fn user_lists_override_heuristics() {
        let mut b = BehaviorConfig::default();
        b.game_apps.push("emulator.exe".into());
        b.ignored_apps.push("game.exe".into());
        assert!(is_game(
            &window("Emulator.exe", r"C:\x\Emulator.exe", false),
            &b,
            1
        ));
        assert!(!is_game(
            &window("game.exe", r"D:\Games\game.exe", true),
            &b,
            1
        ));
        assert!(!is_game(
            &window("KeePassXC.exe", r"C:\x\KeePassXC.exe", true),
            &b,
            1
        ));
    }

    #[test]
    fn minecraft_counts_even_windowed() {
        let b = BehaviorConfig::default();
        let mut java = window("javaw.exe", r"C:\Program Files\Java\bin\javaw.exe", false);
        for title in [
            "Minecraft 1.6.4",
            "Minecraft* 1.21.1 - Multiplayer (3rd-party Server)",
        ] {
            java.title = title.into();
            assert!(is_game(&java, &b, 1), "{title}");
        }
        java.title = "Minecraft Launcher 1.6.93".into();
        assert!(!is_game(&java, &b, 1));
        java.title = "Some Java Tool".into();
        assert!(!is_game(&java, &b, 1));
        let bedrock = window(
            "Minecraft.Windows.exe",
            r"C:\Program Files\WindowsApps\Microsoft.MinecraftUWP\Minecraft.Windows.exe",
            false,
        );
        assert!(is_game(&bedrock, &b, 1));
        assert_eq!(
            display_name(Path::new(r"C:\Java\bin\javaw.exe"), "Minecraft 1.6.4"),
            "Minecraft"
        );
    }

    #[test]
    fn title_names() {
        assert_eq!(
            title_name("Team Fortress 2 - Direct3D 9 - 64 Bit").as_deref(),
            Some("Team Fortress 2")
        );
        assert_eq!(title_name("Celeste").as_deref(), Some("Celeste"));
        assert_eq!(title_name("  - x"), None);
    }

    #[test]
    fn ignores_own_process_shell_and_hidden_windows() {
        let b = BehaviorConfig::default();
        let mut w = window("game.exe", r"D:\Games\game.exe", true);
        assert!(!is_game(&w, &b, 42));
        w.class = "WorkerW".into();
        assert!(!is_game(&w, &b, 1));
        w.class = "SomeClass".into();
        w.visible = false;
        assert!(!is_game(&w, &b, 1));
    }
}
