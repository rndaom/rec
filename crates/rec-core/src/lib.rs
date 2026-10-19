//! Shared building blocks for rec.
//!
//! Everything here is platform-independent and unit-tested: configuration,
//! per-game profiles, hotkey and controller combo parsing, the IPC protocol
//! spoken between `recd`, the `rec` CLI and the settings UI, timestamps, the
//! in-memory replay buffer of encoded packets, and the clips folder on disk
//! (starring and quota pruning). Windows-specific code (capture, encode,
//! audio, overlay) lives in `recd`.

pub mod config;
pub mod controller;
pub mod hotkey;
pub mod ipc;
pub mod media;
pub mod profile;
pub mod replay;
pub mod store;
pub mod time;
