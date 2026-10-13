//! Audio capture for rec: WASAPI (system loopback, one app's audio, or a
//! microphone) encoded to AAC-LC with the Media Foundation AAC encoder that
//! ships with Windows, optionally with a mixed track.
//!
//! One [`AudioCapture`] owns one thread that captures every configured
//! [`AudioInput`], timestamps it, encodes each input to its own track, mixes
//! the chosen inputs into a mixed track, and hands finished
//! [`Packet`](rec_core::replay::Packet)s to a sink. Packets follow the packet
//! contract in [`rec_core::media`]: raw AAC-LC access units of 1024 samples,
//! `keyframe = true`, `pts == dts`, `segment = 0`, timestamps on the QPC
//! clock.
//!
//! # Fixed output format
//!
//! Every track is **AAC-LC, 48 kHz, stereo**, whatever the device mix format
//! is (44.1/48/96 kHz, mono to 7.1, int or float). WASAPI is opened in shared
//! mode with `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM |
//! AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY` and asked for 48 kHz / 16-bit /
//! stereo PCM, so the Windows audio engine's own channel matrixer and sample
//! rate converter do the conversion. A device switch therefore never changes
//! the track format, and process loopback (where the client must name the
//! format) works the same way.
//!
//! # Per-app audio
//!
//! [`AudioSource::Process`] uses *process loopback*
//! (`AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`, Windows 10 2004+): the
//! audio one process tree plays, or everything except it. No handle to the
//! target is needed for capture; rec only opens it with
//! `PROCESS_QUERY_LIMITED_INFORMATION` to notice when it exits. Measured on
//! Windows 11 26200:
//!
//! * The stream delivers continuously, with zeros while the app is silent and
//!   after it exits, and valid QPC timestamps (~10 ms old when read).
//! * The device position is always 0, so continuity is judged from the
//!   timestamps instead.
//! * Its internal queue holds only ~30 ms: polled every 40 ms it lost 25% of
//!   the audio. Process loopback streams are therefore event-driven.
//! * When the app starts or stops playing, the timestamps jump by 15-50 ms
//!   with no discontinuity flag; such a jump is treated as a discontinuity so
//!   the timeline places the next packet by its timestamp.
//!
//! # Timing
//!
//! Timestamps come from WASAPI's `u64QPCPosition` and then advance by sample
//! count; silence fill, discontinuities, drift and long gaps are handled by the
//! pure [`timeline`] module. Loopback data is stamped 10-20 ms *after* the
//! moment we read it (measured here; consistent with a presentation time,
//! i.e. when it is heard), microphone data before it.
//! Packet timestamps account for the encoder delay ([`ENCODER_DELAY_SAMPLES`],
//! measured as 0), so the muxer must **not** add a priming edit list. The
//! mixed track lines the inputs up on their shared QPC timeline with the pure
//! [`mixer`] module.
//!
//! # Threads and COM
//!
//! The caller must call `MFStartup` once per process before
//! [`AudioCapture::start`]. The capture thread initialises COM (MTA) itself,
//! registers with MMCSS ("Audio"), wakes on process loopback data and
//! otherwise polls WASAPI every 40 ms (see `capture.rs` for the measurements
//! behind that). The sink runs on the capture thread, so it must be quick
//! (push into the replay buffer and return).

pub mod aac;
pub mod mixer;
pub mod timeline;

#[cfg(windows)]
mod capture;
#[cfg(windows)]
mod encoder;
#[cfg(windows)]
mod process;
#[cfg(windows)]
mod sys;
#[cfg(windows)]
mod wasapi;

#[cfg(windows)]
pub use capture::AudioCapture;
#[cfg(windows)]
pub use process::process_loopback_supported;
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
    /// What one app and every process it started plays (`exclude = false`),
    /// or everything *except* that app (`exclude = true`). Needs
    /// [`process_loopback_supported`].
    Process {
        target: ProcessTarget,
        exclude: bool,
    },
}

/// The app a [`AudioSource::Process`] input follows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessTarget {
    /// One process (and its children), e.g. the game. Silence once it exits.
    Pid(u32),
    /// The first of these executables (e.g. `"Discord.exe"`) that is running,
    /// in list order, matched case-insensitively. Checked every few seconds,
    /// so the app may start, quit or restart while capturing.
    Executables(Vec<String>),
}

impl AudioSource {
    fn follows_default(&self) -> bool {
        !matches!(self, AudioSource::Microphone { device_id: Some(_) })
    }
}

/// One thing to capture, and where its audio goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioInput {
    pub source: AudioSource,
    /// Its own track, or `None` to only hear it in the mixed track.
    pub track: Option<TrackId>,
    /// Also mix it into [`AudioCaptureConfig::mix_track`].
    pub mix: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioCaptureConfig {
    pub inputs: Vec<AudioInput>,
    /// A track that sums every input with `mix = true`.
    pub mix_track: Option<TrackId>,
    /// Requested AAC bitrate per track; mapped to the nearest one the encoder
    /// supports (96, 128, 160 or 192 kbit/s, see [`aac::nearest_bitrate_kbps`]).
    pub bitrate_kbps: u32,
}

impl AudioCaptureConfig {
    /// One input on one track, nothing mixed.
    pub fn single(source: AudioSource, track: TrackId, bitrate_kbps: u32) -> Self {
        Self {
            inputs: vec![AudioInput {
                source,
                track: Some(track),
                mix: false,
            }],
            mix_track: None,
            bitrate_kbps,
        }
    }

    /// Every track id this capture writes, mixed track first.
    pub fn tracks(&self) -> Vec<TrackId> {
        self.mix_track
            .into_iter()
            .chain(self.inputs.iter().filter_map(|i| i.track))
            .collect()
    }

    fn validate(&self) -> Result<(), AudioError> {
        let invalid = |msg: &str| Err(AudioError::Config(msg.into()));
        if self.inputs.is_empty() {
            return invalid("no audio inputs");
        }
        let mixed = self.inputs.iter().filter(|i| i.mix).count();
        if self.mix_track.is_some() && mixed == 0 {
            return invalid("a mixed track needs at least one mixed input");
        }
        if self
            .inputs
            .iter()
            .any(|i| i.track.is_none() && !(i.mix && self.mix_track.is_some()))
        {
            return invalid("an input has neither its own track nor the mix");
        }
        let mut tracks = self.tracks();
        let count = tracks.len();
        tracks.sort_unstable();
        tracks.dedup();
        if tracks.len() != count {
            return invalid("track ids must be unique");
        }
        Ok(())
    }
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

/// Counters of one input; see [`AudioStats::inputs`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InputStats {
    /// Packets of this input's own track handed to the sink.
    pub packets: u64,
    /// Of `packets`, silent packets repeated instead of encoded.
    pub repeated_silent: u64,
    /// Silence inserted: idle loopback, missing device, gaps, drift
    /// correction.
    pub silence_ms: u64,
    /// Captured audio dropped (overlap with already filled silence, drift
    /// correction).
    pub dropped_ms: u64,
    pub drift_corrections: u32,
    /// Gaps longer than the fill limit (system sleep) bridged by a timestamp
    /// jump.
    pub long_gaps: u32,
    /// Times capture was reopened (default device change, device lost, the
    /// followed app restarted).
    pub restarts: u32,
    /// What is being captured: a device's friendly name or an app like
    /// `Discord.exe (pid 1234)`. `None` while nothing is available (the input
    /// then carries silence).
    pub device: Option<String>,
    /// Most recent device error.
    pub last_error: Option<String>,
}

/// Counters of one capture; see [`AudioCapture::stats`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioStats {
    /// In [`AudioCaptureConfig::inputs`] order.
    pub inputs: Vec<InputStats>,
    /// Packets of the mixed track handed to the sink.
    pub mix_packets: u64,
    /// Of `mix_packets`, silent packets repeated instead of encoded.
    pub mix_repeated_silent: u64,
    /// The error that stopped the thread (encoder failure).
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
    #[error("invalid audio configuration: {0}")]
    Config(String),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn input(track: Option<TrackId>, mix: bool) -> AudioInput {
        AudioInput {
            source: AudioSource::SystemLoopback,
            track,
            mix,
        }
    }

    #[test]
    fn config_validation() {
        let ok = AudioCaptureConfig {
            inputs: vec![input(Some(2), true), input(None, true)],
            mix_track: Some(1),
            bitrate_kbps: 160,
        };
        assert!(ok.validate().is_ok());
        assert_eq!(ok.tracks(), vec![1, 2]);
        assert!(
            AudioCaptureConfig::single(AudioSource::SystemLoopback, 1, 160)
                .validate()
                .is_ok()
        );

        let bad = [
            AudioCaptureConfig {
                inputs: vec![],
                ..ok.clone()
            },
            // Mixed track without mixed inputs.
            AudioCaptureConfig {
                inputs: vec![input(Some(2), false)],
                ..ok.clone()
            },
            // Input going nowhere.
            AudioCaptureConfig {
                inputs: vec![input(None, true)],
                mix_track: None,
                ..ok.clone()
            },
            // Duplicate track ids.
            AudioCaptureConfig {
                inputs: vec![input(Some(1), true)],
                ..ok.clone()
            },
        ];
        for config in bad {
            assert!(matches!(config.validate(), Err(AudioError::Config(_))));
        }
    }
}
