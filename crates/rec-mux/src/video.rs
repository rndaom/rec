//! Video access unit conversion (Annex-B to MP4 samples) and codec
//! configuration records.

use rec_core::media::VideoCodec;

use crate::annexb::{push_length_prefixed, split_nals};
use crate::error::MuxError;
use crate::{av1, h264, h265};

/// How the muxer treats a NAL unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NalClass {
    Vps,
    Sps,
    Pps,
    /// Access unit delimiter; dropped.
    Aud,
    /// Everything else (slices, SEI, ...); kept in the sample.
    Other,
}

/// Parameter sets found in one access unit, deduplicated, in stream order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ParamSets {
    pub vps: Vec<Vec<u8>>,
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
    /// AV1 sequence header OBUs.
    pub sequence_headers: Vec<Vec<u8>>,
}

impl ParamSets {
    fn add(list: &mut Vec<Vec<u8>>, nal: &[u8]) {
        if !list.iter().any(|existing| existing == nal) {
            list.push(nal.to_vec());
        }
    }
}

/// An access unit converted to a sample.
#[derive(Debug)]
pub(crate) struct Converted {
    /// 4-byte length-prefixed NAL units, parameter sets and AUDs removed.
    pub sample: Vec<u8>,
    pub params: ParamSets,
}

/// Converts one packet to an MP4 sample, collecting the parameter sets it
/// contained. H.264 and HEVC arrive as Annex-B access units and become
/// length-prefixed NAL units; AV1 arrives as a low-overhead OBU temporal unit.
pub(crate) fn convert_access_unit(codec: VideoCodec, data: &[u8]) -> Result<Converted, MuxError> {
    let classify = match codec {
        VideoCodec::H264 => h264::classify,
        VideoCodec::Hevc => h265::classify,
        VideoCodec::Av1 => {
            let c = av1::convert_temporal_unit(data)?;
            return Ok(Converted {
                sample: c.sample,
                params: ParamSets {
                    sequence_headers: c.sequence_headers,
                    ..ParamSets::default()
                },
            });
        }
    };
    let nals = split_nals(data)?;
    let mut sample = Vec::with_capacity(data.len());
    let mut params = ParamSets::default();
    for nal in nals {
        match classify(nal) {
            NalClass::Vps => ParamSets::add(&mut params.vps, nal),
            NalClass::Sps => ParamSets::add(&mut params.sps, nal),
            NalClass::Pps => ParamSets::add(&mut params.pps, nal),
            NalClass::Aud => {}
            NalClass::Other => push_length_prefixed(&mut sample, nal)?,
        }
    }
    if sample.is_empty() {
        return Err(MuxError::malformed(
            "video packet",
            "access unit contains no picture data",
        ));
    }
    Ok(Converted { sample, params })
}

/// The sample entry and codec configuration box for the file's `stsd`.
#[derive(Clone, Debug)]
pub(crate) struct CodecConfig {
    /// Sample entry fourcc (`avc1`, `hvc1`, `av01`).
    pub entry: [u8; 4],
    /// Configuration box fourcc (`avcC`, `hvcC`, `av1C`).
    pub config_box: [u8; 4],
    pub config: Vec<u8>,
}

/// Builds the codec configuration from the first keyframe's parameter sets.
pub(crate) fn build_config(codec: VideoCodec, params: &ParamSets) -> Result<CodecConfig, MuxError> {
    match codec {
        VideoCodec::H264 => Ok(CodecConfig {
            entry: *b"avc1",
            config_box: *b"avcC",
            config: h264::build_avcc(&params.sps, &params.pps)?,
        }),
        VideoCodec::Hevc => Ok(CodecConfig {
            entry: *b"hvc1",
            config_box: *b"hvcC",
            config: h265::build_hvcc(&params.vps, &params.sps, &params.pps)?,
        }),
        VideoCodec::Av1 => Ok(CodecConfig {
            entry: *b"av01",
            config_box: *b"av1C",
            config: av1::build_av1c(&params.sequence_headers)?,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_parameter_sets_and_aud_but_keeps_sei_and_slices() {
        let data = [
            0, 0, 0, 1, 0x09, 0xF0, // AUD
            0, 0, 0, 1, 0x67, 1, 2, 3, // SPS
            0, 0, 1, 0x68, 4, 5, // PPS
            0, 0, 1, 0x06, 6, 7, // SEI
            0, 0, 0, 1, 0x65, 8, 9, 10, 11, // IDR slice
        ];
        let c = convert_access_unit(VideoCodec::H264, &data).unwrap();
        assert_eq!(c.params.sps, [vec![0x67, 1, 2, 3]]);
        assert_eq!(c.params.pps, [vec![0x68, 4, 5]]);
        assert!(c.params.vps.is_empty());
        assert_eq!(
            c.sample,
            [0, 0, 0, 3, 0x06, 6, 7, 0, 0, 0, 5, 0x65, 8, 9, 10, 11]
        );
    }

    #[test]
    fn hevc_strips_vps_sps_pps_aud() {
        let data = [
            0, 0, 0, 1, 0x46, 0x01, 0x50, // AUD
            0, 0, 0, 1, 0x40, 0x01, 0xAA, // VPS
            0, 0, 0, 1, 0x42, 0x01, 0xBB, // SPS
            0, 0, 0, 1, 0x44, 0x01, 0xCC, // PPS
            0, 0, 0, 1, 0x26, 0x01, 0xDD, 0xEE, // IDR_W_RADL
        ];
        let c = convert_access_unit(VideoCodec::Hevc, &data).unwrap();
        assert_eq!(c.params.vps.len(), 1);
        assert_eq!(c.params.sps.len(), 1);
        assert_eq!(c.params.pps.len(), 1);
        assert_eq!(c.sample, [0, 0, 0, 4, 0x26, 0x01, 0xDD, 0xEE]);
    }

    #[test]
    fn duplicate_parameter_sets_are_collapsed() {
        let data = [
            0, 0, 1, 0x67, 1, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x67, 2, 0, 0, 1, 0x65, 9,
        ];
        let c = convert_access_unit(VideoCodec::H264, &data).unwrap();
        assert_eq!(c.params.sps, [vec![0x67, 1], vec![0x67, 2]]);
    }

    #[test]
    fn access_unit_without_pictures_is_an_error() {
        let data = [0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2];
        assert!(convert_access_unit(VideoCodec::H264, &data).is_err());
        assert!(convert_access_unit(VideoCodec::H264, &[1, 2, 3]).is_err());
    }
}
