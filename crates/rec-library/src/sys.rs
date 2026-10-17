//! Process setup and well-known folders.

use std::path::PathBuf;

use rec_core::config::{self, Config};
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_LITE, MFStartup};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Threading::{
    GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
    ProcessPowerThrottling, SetProcessInformation,
};
use windows::Win32::UI::Shell::{FOLDERID_Videos, KF_FLAG_DEFAULT, SHGetKnownFolderPath};

/// COM (multithreaded) and Media Foundation for the calling thread and the
/// process. Call once on each thread that decodes or writes images; both
/// stay initialised until the process exits.
pub fn init() -> Result<(), String> {
    // SAFETY: plain initialisation calls; S_FALSE (already initialised) is
    // success. They are never balanced: the process exits instead.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .map_err(|e| format!("CoInitializeEx: {e}"))?;
        // MFSTARTUP_LITE: no sockets, so decoding can't reach the network.
        MFStartup(MF_VERSION, MFSTARTUP_LITE).map_err(|e| format!("MFStartup: {e}"))?;
    }
    Ok(())
}

/// EcoQoS for the whole process: batch work (`rec digest`, `rec analyze`)
/// then stays on efficient cores and at low clocks, out of a game's way.
pub fn background() {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    };
    // SAFETY: `state` is a valid PROCESS_POWER_THROTTLING_STATE of the size
    // passed. Failure (older Windows) only loses the hint.
    let _ = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            (&raw const state).cast(),
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    };
}

/// `%LOCALAPPDATA%\rec`, where recd keeps logs and session journals.
pub fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
        .join("rec")
}

/// The folder recd saves clips to with the user's config, as recd resolves
/// it (`output.directory`, else `<Videos>\rec`).
pub fn clips_dir() -> PathBuf {
    let config = Config::load(&config::default_config_path()).unwrap_or_default();
    if let Some(dir) = config.output.directory {
        return dir;
    }
    videos_dir().map_or_else(config::fallback_output_dir, |videos| videos.join("rec"))
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
