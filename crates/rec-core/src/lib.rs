//! Shared building blocks for rec.
//!
//! Everything here is platform-independent and unit-tested: configuration,
//! hotkey parsing, the IPC protocol spoken between `recd`, the `rec` CLI and
//! the settings UI, timestamps, and the in-memory replay buffer of encoded
//! packets. Windows-specific code (capture, encode, audio, overlay) lives in
//! `recd`.

pub mod config;
pub mod hotkey;
pub mod ipc;
pub mod replay;
pub mod time;
