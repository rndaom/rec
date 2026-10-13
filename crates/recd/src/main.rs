//! `recd`: the rec background service.
//!
//! Milestone 0 only loads, validates and (on first run) writes the config.
//! Capture, encoding, the replay buffer wiring, hotkeys, the IPC server and the
//! tray icon arrive in milestone 1; see `docs/roadmap.md`.

use std::process::ExitCode;

use rec_core::config::{self, Config};

fn main() -> ExitCode {
    let path = config::default_config_path();
    let config = match Config::load(&path) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("recd: {e}");
            return ExitCode::FAILURE;
        }
    };
    if !path.exists() {
        if let Err(e) = config.save(&path) {
            eprintln!("recd: {e}");
            return ExitCode::FAILURE;
        }
        eprintln!("recd: wrote default config to {}", path.display());
    }

    let hotkeys = &config.hotkeys;
    let bindings = [
        ("save clip", Some(&hotkeys.save_clip)),
        ("overlay", Some(&hotkeys.toggle_overlay)),
    ]
    .into_iter()
    .chain(
        hotkeys
            .extra_clips
            .iter()
            .map(|c| ("extra clip", Some(&c.hotkey))),
    )
    .chain([
        ("marker", hotkeys.add_marker.as_ref()),
        ("pause", hotkeys.pause.as_ref()),
    ]);
    for (action, hotkey) in bindings {
        if let Some(owner) = hotkey.and_then(|h| h.known_conflict()) {
            eprintln!(
                "recd: warning: {action} hotkey {} is also used by {owner}",
                hotkey.unwrap()
            );
        }
    }

    eprintln!(
        "recd: config ok ({} s replay, {} fps). Capture is not implemented yet (milestone 1).",
        config.replay.seconds, config.video.fps
    );
    ExitCode::SUCCESS
}
