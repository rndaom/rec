//! Saved clips as an AI (or the library UI) sees them: phase B of
//! `docs/ai-clips.md`.
//!
//! - [`clip`] reads `<clip>.rec.json` sidecars, and [`journal`] the session
//!   journals in `%LOCALAPPDATA%\rec\sessions`.
//! - [`library`] finds clips under folders and fills gaps from the journal
//!   (clips saved with `output.sidecar = false` are only in the journal).
//! - [`overlap`] relates clips whose session ranges intersect.
//! - [`timeline`] merges sidecar events with loud moments found in the
//!   loudness curves; [`summary`] writes it as plain language.
//! - [`decode`] pulls frames out of a clip with Media Foundation (hardware
//!   decoding through D3D11), and [`image`] scales, labels and saves them as
//!   JPEG with WIC and Direct2D. No FFmpeg (D4).
//! - [`analysis`] is layer 2: thumbnail, scene cuts, brightness and motion,
//!   computed after saving and kept in the sidecar.
//! - [`digest`] writes the self-contained bundle of `rec digest`.
//! - [`trim`] cuts a clip losslessly at its GOP fragments, with a sidecar
//!   whose `parent` is the source.
//!
//! Nothing here runs inside `recd`, so none of it costs anything while a
//! game is being recorded (D11).

pub mod analysis;
pub mod b64;
pub mod clip;
pub mod decode;
pub mod digest;
pub mod image;
pub mod journal;
pub mod library;
pub mod overlap;
pub mod summary;
pub mod sys;
pub mod time;
pub mod timeline;
pub mod trim;

pub use clip::Clip;
pub use library::Library;
