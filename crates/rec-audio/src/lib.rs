//! Audio capture for rec: WASAPI (system loopback or a microphone) encoded to
//! AAC-LC with the Media Foundation AAC encoder that ships with Windows.
//!
//! Each [`AudioCapture`] owns one thread that captures, timestamps and encodes,
//! and hands finished [`Packet`](rec_core::replay::Packet)s to a sink. Packets
//! follow the packet contract in [`rec_core::media`]: raw AAC-LC access units
//! of 1024 samples, `keyframe = true`, `pts == dts`, `segment = 0`, timestamps
//! on the QPC clock.
//!
//! # Fixed output format
//!
//! The track is always **AAC-LC, 48 kHz, stereo**, whatever the device mix
//! format is (44.1/48/96 kHz, mono to 7.1, int or float). WASAPI is opened in
//! shared mode with `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM |
//! AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY` and asked for 48 kHz / 16-bit /
//! stereo PCM, so the Windows audio engine's own channel matrixer and sample
//! rate converter do the conversion. A device switch therefore never changes
//! the track format.
//!
//! This replaces the Media Foundation Audio Resampler DSP: it is the same
//! engine converter every shared-mode stream already uses, needs no second
//! MFT, no per-buffer `IMFSample` and no latency bookkeeping (WASAPI positions
//! and timestamps are already in 48 kHz output frames), and it is exactly the
//! model process loopback (`PROCESS_LOOPBACK`) uses, where the client names
//! the format and the engine converts.
//!
//! # Timing
//!
//! Timestamps come from WASAPI's `u64QPCPosition` and then advance by sample
//! count; silence fill, discontinuities, drift and long gaps are handled by the
//! pure [`timeline`] module. Loopback data is stamped 10-20 ms *after* the
//! moment we read it (measured here; consistent with a presentation time,
//! i.e. when it is heard), microphone data before it.
//! Packet timestamps account for the encoder delay ([`ENCODER_DELAY_SAMPLES`],
//! measured as 0), so the muxer must **not** add a priming edit list.
//!
//! # Threads and COM
//!
//! The caller must call `MFStartup` once per process before
//! [`AudioCapture::start`]. The capture thread initialises COM (MTA) itself,
//! registers with MMCSS ("Audio") and polls WASAPI every 40 ms (see
//! `capture.rs` for the measurements behind that). The sink runs on the
//! capture thread, so it must be quick (push into the replay buffer and
//! return).

pub mod aac;
pub mod timeline;

#[cfg(windows)]
mod capture;
#[cfg(windows)]
mod encoder;
#[cfg(windows)]
mod sys;
#[cfg(windows)]
mod wasapi;

#[cfg(windows)]
pub use capture::AudioCapture;
#[cfg(windows)]
pub use wasapi::list_microphones;

use rec_core::replay::TrackId;

/// Output sample rate of every audio track.
pub const SAMPLE_RATE: u32 = timeline::SAMPLE_RATE;
/// Output channel count of every audio track.
pub const CHANNELS: u16 = 2;
/// Samples per AAC-LC access unit.
pub const SAMPLES_PER_PACKET: u32 = 1024;

/// Delay of the Media Foundation AAC encoder, in samples: decoding the
/// emitted packets from the first one, output sample `n` corresponds to input
/// sample `n - ENCODER_DELAY_SAMPLES`. Packet `i` is stamped with the capture
/// time of input sample `i * 1024 - ENCODER_DELAY_SAMPLES`.
///
/// Measured as **0** on Windows 11 26200 by the `encoder_delay` test
/// (`cargo test -p rec-audio --lib -- --ignored --nocapture encoder_delay`):
/// impulses at known positions (including one inside the first packet) are
/// encoded, the ADTS stream is decoded with ffmpeg (which outputs every
/// decoded sample; ADTS carries no priming info) and every peak lands at
/// exactly its input position. The MF encoder already swallows its priming
/// internally: 192 000 input samples yield exactly 188 packets
/// (`ceil(192000 / 1024)`). The muxer must therefore write no priming edit
/// list for these tracks.
pub const ENCODER_DELAY_SAMPLES: i64 = 0;

/// What to capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioSource {
    /// Everything played on the default render device (follows default device
    /// changes).
    SystemLoopback,
    /// A capture device. `None` is the default *communications* capture device
    /// and follows default device changes; `Some(id)` is a fixed device (an id
    /// from [`list_microphones`]).
    Microphone { device_id: Option<String> },
}

impl AudioSource {
    fn follows_default(&self) -> bool {
        matches!(
            self,
            AudioSource::SystemLoopback | AudioSource::Microphone { device_id: None }
        )
    }

    fn label(&self) -> &'static str {
        match self {
            AudioSource::SystemLoopback => "loopback",
            AudioSource::Microphone { .. } => "mic",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioCaptureConfig {
    pub source: AudioSource,
    /// Track id written into every packet.
    pub track: TrackId,
    /// Requested AAC bitrate; mapped to the nearest one the encoder supports
    /// (96, 128, 160 or 192 kbit/s, see [`aac::nearest_bitrate_kbps`]).
    pub bitrate_kbps: u32,
}

/// A capture endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioDevice {
    /// Endpoint id, stable across reboots; pass it as
    /// [`AudioSource::Microphone`]`{ device_id }`.
    pub id: String,
    pub name: String,
    /// The default communications capture device (what `device_id: None`
    /// records).
    pub is_default: bool,
}

/// Counters of one capture; see [`AudioCapture::stats`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioStats {
    /// Packets handed to the sink.
    pub packets: u64,
    /// Silence inserted into the track: idle loopback, missing device, gaps,
    /// drift correction.
    pub silence_ms: u64,
    /// Captured audio dropped (overlap with already filled silence, drift
    /// correction).
    pub dropped_ms: u64,
    pub drift_corrections: u32,
    /// Gaps longer than the fill limit (system sleep) bridged by a timestamp
    /// jump.
    pub long_gaps: u32,
    /// Times capture was reopened (default device change, device lost).
    pub restarts: u32,
    /// Friendly name of the device being captured; `None` while no device is
    /// available (the track then carries silence).
    pub device: Option<String>,
    /// Most recent device or encoder error.
    pub last_error: Option<String>,
    /// False once the thread has stopped (after `stop` or a fatal encoder
    /// error, which is then in `last_error`).
    pub running: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("{context}: {source}")]
    Windows {
        context: &'static str,
        #[source]
        source: WinError,
    },
    #[error("audio encoder: {0}")]
    Encoder(String),
    #[error("audio device: {0}")]
    Device(String),
    #[error("audio thread: {0}")]
    Thread(String),
}

/// `windows::core::Error` on Windows; a placeholder elsewhere so the error type
/// stays platform-independent.
#[cfg(windows)]
pub type WinError = windows::core::Error;
#[cfg(not(windows))]
pub type WinError = std::io::Error;

#[cfg(windows)]
pub(crate) trait Context<T> {
    fn context(self, context: &'static str) -> Result<T, AudioError>;
}

#[cfg(windows)]
impl<T> Context<T> for windows::core::Result<T> {
    fn context(self, context: &'static str) -> Result<T, AudioError> {
        self.map_err(|source| AudioError::Windows { context, source })
    }
}
