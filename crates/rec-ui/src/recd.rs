//! Talking to recd over its IPC pipe. The app only asks for status, a config
//! reload, and can start recd; it never sends `Quit` or `Pause`, so opening
//! or closing it can't change what is being recorded.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use rec_core::ipc::{self, RecorderState, Request, Response, Status};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct RecdState {
    pub running: bool,
    /// A game may be running ([`busy`]): heavy work should wait or warn.
    pub busy: bool,
    pub status: Option<Status>,
    pub error: Option<String>,
}

pub fn state() -> RecdState {
    match ipc::send(&Request::Status) {
        Ok(Response::Status(status)) => RecdState {
            running: true,
            busy: busy(&status),
            status: Some(status),
            error: None,
        },
        Ok(other) => RecdState {
            running: true,
            busy: true,
            status: None,
            error: Some(format!("unexpected answer: {other:?}")),
        },
        Err(e) if e == ipc::NOT_RUNNING => RecdState {
            running: false,
            busy: false,
            status: None,
            error: None,
        },
        Err(e) => RecdState {
            running: false,
            busy: true,
            status: None,
            error: Some(e),
        },
    }
}

/// Whether a game may be running, so background work (analysis) must wait
/// and exports should warn. Only "recd isn't running" or "recd is idle
/// with nothing to capture" count as free: a paused or failing recd may
/// still have a game in front, and an unanswered pipe proves nothing.
pub fn game_active() -> bool {
    match ipc::send(&Request::Status) {
        Ok(Response::Status(status)) => busy(&status),
        Err(e) if e == ipc::NOT_RUNNING => false,
        _ => true,
    }
}

pub fn busy(status: &Status) -> bool {
    status.state != RecorderState::Idle || status.target.is_some()
}

/// Asks a running recd to re-read the settings. `None` when recd isn't
/// running (it reads them when it starts) or took them; otherwise its error.
pub fn reload() -> Option<String> {
    match ipc::send(&Request::ReloadConfig) {
        Ok(Response::Error { message }) => Some(message),
        Ok(_) => None,
        Err(e) if e == ipc::NOT_RUNNING => None,
        Err(e) => Some(e),
    }
}

/// Starts recd from next to this app (else from `PATH`), detached so it
/// outlives the app.
pub fn start() -> Result<(), String> {
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("recd.exe")))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("recd.exe"));
    let spawn = |flags| {
        Command::new(&exe)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(flags)
            .spawn()
    };
    let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    // Leave the app's job, if it has one, so closing the app can't take
    // recd with it; some jobs forbid breaking away.
    spawn(flags | CREATE_BREAKAWAY_FROM_JOB)
        .or_else(|_| spawn(flags))
        .map(drop)
        .map_err(|e| format!("can't start {}: {e}", exe.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: RecorderState, target: Option<&str>) -> Status {
        Status {
            protocol: ipc::PROTOCOL_VERSION,
            version: String::new(),
            state,
            reason: None,
            target: target.map(str::to_owned),
            encoder: None,
            buffered_seconds: 0.0,
            buffer_bytes: 0,
            warning: None,
        }
    }

    #[test]
    fn only_idle_without_a_game_is_free() {
        assert!(!busy(&status(RecorderState::Idle, None)));
        assert!(busy(&status(RecorderState::Idle, Some("game.exe"))));
        assert!(busy(&status(RecorderState::Recording, Some("game.exe"))));
        // Paused (by hand, or a private app in front) and failing captures
        // may still have a game running.
        assert!(busy(&status(RecorderState::Paused, None)));
        assert!(busy(&status(RecorderState::Error, None)));
    }
}
