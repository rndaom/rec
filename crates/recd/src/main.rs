//! `recd`: the rec background service.
//!
//! Lives in the notification area, keeps the replay buffer filled while a game
//! is running, and saves a clip when the hotkey is pressed. See
//! `docs/decisions.md` for why it is built the way it is.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod detect;
mod feedback;
mod hotkeys;
mod ipc_server;
mod journal;
mod log;
mod mux;
mod overlay;
mod panel;
mod paths;
mod recover;
mod saver;
mod session;
mod sidecar;
mod tray;
mod watch;
mod win;

use std::process::ExitCode;

fn main() -> ExitCode {
    log::init();
    match app::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("fatal: {e}");
            ExitCode::FAILURE
        }
    }
}
