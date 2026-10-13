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
//! * **Video** (AV1): one temporal unit per packet in the low-overhead OBU
//!   format (every OBU has `obu_has_size_field = 1`), no hidden or reordered
//!   frames. Every keyframe packet is a key frame and **carries the sequence
//!   header OBU**, so the muxer can build `av1C` from it.
//! * **HDR10** ([`VideoColor::Hdr10`]): every keyframe packet also carries the
//!   static metadata in-band (HEVC prefix SEI `mastering_display_colour_volume`
//!   and `content_light_level_info`; AV1 metadata OBUs `HDR_MDCV` and
//!   `HDR_CLL`), so a cut clip or a remux keeps it.
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
    pub color: VideoColor,
}

impl VideoFormat {
    /// Bits per sample of the encoded picture.
    pub fn bit_depth(&self) -> u8 {
        self.color.bit_depth()
    }
}

/// Colour description of the encoded picture. Both are 4:2:0 YCbCr in
/// limited ("studio") range with left-sited chroma.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VideoColor {
    /// 8-bit BT.709: primaries, transfer and matrix (sRGB desktop content).
    #[default]
    Sdr,
    /// 10-bit HDR10: BT.2020 primaries, SMPTE ST 2084 (PQ) transfer,
    /// BT.2020 non-constant-luminance matrix, plus static metadata.
    Hdr10(HdrMetadata),
}

impl VideoColor {
    pub fn bit_depth(self) -> u8 {
        match self {
            VideoColor::Sdr => 8,
            VideoColor::Hdr10(_) => 10,
        }
    }

    pub fn is_hdr(self) -> bool {
        matches!(self, VideoColor::Hdr10(_))
    }

    /// ISO/IEC 23091-2 (H.273) code points: `(colour_primaries,
    /// transfer_characteristics, matrix_coefficients)`, as written into the
    /// bitstream's VUI / sequence header and the MP4 `colr` box.
    pub fn cicp(self) -> (u8, u8, u8) {
        match self {
            VideoColor::Sdr => (1, 1, 1),
            VideoColor::Hdr10(_) => (9, 16, 9),
        }
    }
}

/// SMPTE ST 2086 mastering display colour volume plus CTA-861.3 content
/// light levels, in the units the HEVC SEI, the AV1 metadata OBUs and the MP4
/// `mdcv` / `clli` boxes all use.
///
/// rec records what the display showed, so the "mastering display" is the
/// user's monitor as DXGI reports it, and the light levels are bounded by its
/// peak (the converter clips to it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HdrMetadata {
    /// CIE 1931 xy chromaticity of the red, green and blue primaries and the
    /// white point, in units of 0.00002.
    pub red: [u16; 2],
    pub green: [u16; 2],
    pub blue: [u16; 2],
    pub white: [u16; 2],
    /// Mastering display luminance in units of 0.0001 cd/m².
    pub max_luminance: u32,
    pub min_luminance: u32,
    /// Maximum content light level and maximum frame-average light level in
    /// cd/m²; 0 = unknown.
    pub max_cll: u16,
    pub max_fall: u16,
}

impl HdrMetadata {
    /// From floating-point display values: xy chromaticities and cd/m².
    pub fn from_display(
        red: [f32; 2],
        green: [f32; 2],
        blue: [f32; 2],
        white: [f32; 2],
        max_nits: f32,
        min_nits: f32,
        max_frame_average_nits: f32,
    ) -> Self {
        let xy = |c: [f32; 2]| c.map(|v| (v.clamp(0.0, 1.0) * 50_000.0).round() as u16);
        let nits = |v: f32| (v.max(0.0) * 10_000.0).round().min(u32::MAX as f32) as u32;
        let level = |v: f32| v.max(0.0).round().min(f32::from(u16::MAX)) as u16;
        Self {
            red: xy(red),
            green: xy(green),
            blue: xy(blue),
            white: xy(white),
            max_luminance: nits(max_nits),
            min_luminance: nits(min_nits),
            max_cll: level(max_nits),
            max_fall: level(max_frame_average_nits.min(max_nits)),
        }
    }

    /// BT.2020 primaries, D65 white, 1000 / 0.0001 cd/m², no light levels:
    /// the conventional values when the display reports nothing usable.
    pub fn bt2020_1000() -> Self {
        Self::from_display(
            [0.708, 0.292],
            [0.170, 0.797],
            [0.131, 0.046],
            [0.3127, 0.3290],
            1000.0,
            0.0001,
            0.0,
        )
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdr_metadata_units() {
        let m = HdrMetadata::from_display(
            [0.708, 0.292],
            [0.17, 0.797],
            [0.131, 0.046],
            [0.3127, 0.329],
            603.7,
            0.0491,
            276.8,
        );
        assert_eq!(m.red, [35_400, 14_600]);
        assert_eq!(m.white, [15_635, 16_450]);
        assert_eq!(m.max_luminance, 6_037_000);
        assert_eq!(m.min_luminance, 491);
        assert_eq!((m.max_cll, m.max_fall), (604, 277));
    }

    #[test]
    fn hdr_metadata_clamps_nonsense() {
        let m = HdrMetadata::from_display(
            [2.0, -1.0],
            [0.0; 2],
            [0.0; 2],
            [0.0; 2],
            -5.0,
            f32::NAN,
            1e9,
        );
        assert_eq!(m.red, [50_000, 0]);
        assert_eq!((m.max_luminance, m.max_cll, m.max_fall), (0, 0, 0));
    }

    #[test]
    fn colour_code_points() {
        assert_eq!(VideoColor::Sdr.cicp(), (1, 1, 1));
        let hdr = VideoColor::Hdr10(HdrMetadata::bt2020_1000());
        assert_eq!(hdr.cicp(), (9, 16, 9));
        assert_eq!((hdr.bit_depth(), VideoColor::Sdr.bit_depth()), (10, 8));
    }
}
