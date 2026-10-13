//! Stream descriptions shared by the capture/encode crates, the replay buffer
//! and the muxer.
//!
//! # Packet contract
//!
//! Every [`Packet`](crate::replay::Packet) pushed into the replay buffer
//! follows these rules, so any snapshot starting at a video keyframe can be
//! muxed on its own:
//!
//! * **Timestamps** (`pts`, `dts`, `duration`) are [`Hns`](crate::time::Hns) on
//!   the QPC clock ([`crate::time::qpc_now`]), shared by all tracks. They are
//!   absolute, not rebased; the muxer rebases a clip to start at zero.
//! * **Video** (`TrackKind::Video`): H.264 / HEVC as an Annex-B byte stream
//!   (start codes), one access unit per packet, no B-frames (`pts == dts`).
//!   Every keyframe packet is an IDR **and carries its parameter sets**
//!   (SPS+PPS, plus VPS for HEVC) in-band, so the muxer can build `avcC` /
//!   `hvcC` from whichever keyframe a clip starts on.
//! * **Audio** (`TrackKind::Audio`): raw AAC-LC access units (no ADTS header),
//!   1024 samples each, every packet `keyframe = true`. The decoder config
//!   (AudioSpecificConfig) is in [`AudioFormat::codec_config`].

use crate::replay::TrackId;

/// Track 0 is always the video track.
pub const VIDEO_TRACK: TrackId = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
}

impl VideoCodec {
    pub fn name(self) -> &'static str {
        match self {
            VideoCodec::H264 => "H.264",
            VideoCodec::Hevc => "HEVC",
            VideoCodec::Av1 => "AV1",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoFormat {
    pub codec: VideoCodec,
    /// Encoded size in pixels (even numbers).
    pub width: u32,
    pub height: u32,
    /// Nominal frame rate of the encode tick.
    pub fps: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AudioCodec {
    /// AAC-LC.
    Aac,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u16,
    /// Decoder configuration; for AAC the AudioSpecificConfig (usually 2 bytes).
    pub codec_config: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Video(VideoFormat),
    Audio(AudioFormat),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackInfo {
    pub id: TrackId,
    /// Human-readable name written into the file, e.g. "Game audio".
    pub name: String,
    pub kind: TrackKind,
}
