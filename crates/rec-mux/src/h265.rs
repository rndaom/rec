//! HEVC: NAL classification and `hvcC` construction.

use crate::bits::{BitReader, unescape_rbsp};
use crate::boxes::Put;
use crate::error::MuxError;
use crate::video::NalClass;

pub(crate) const NAL_VPS: u8 = 32;
pub(crate) const NAL_SPS: u8 = 33;
pub(crate) const NAL_PPS: u8 = 34;

pub(crate) fn classify(nal: &[u8]) -> NalClass {
    match nal.first().map(|b| (b >> 1) & 0x3F) {
        Some(NAL_VPS) => NalClass::Vps,
        Some(NAL_SPS) => NalClass::Sps,
        Some(NAL_PPS) => NalClass::Pps,
        Some(35) => NalClass::Aud,
        _ => NalClass::Other,
    }
}

/// Fields of the SPS needed for `hvcC`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SpsInfo {
    pub profile_space: u8,
    pub tier_flag: bool,
    pub profile_idc: u8,
    pub profile_compatibility: u32,
    /// The 48 `general_*_flag` / reserved bits that follow the compatibility flags.
    pub constraint_flags: u64,
    pub level_idc: u8,
    pub chroma_format_idc: u8,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub num_temporal_layers: u8,
    pub temporal_id_nested: bool,
}

pub(crate) fn parse_sps(nal: &[u8]) -> Result<SpsInfo, MuxError> {
    const BAD: &str = "truncated or invalid HEVC SPS";
    let bad = || MuxError::malformed("HEVC SPS", BAD);
    if nal.len() < 3 {
        return Err(bad());
    }
    let rbsp = unescape_rbsp(&nal[2..]);
    let mut r = BitReader::new(&rbsp);
    r.skip(4).ok_or_else(bad)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.bits(3).ok_or_else(bad)?;
    let temporal_id_nested = r.flag().ok_or_else(bad)?;

    // profile_tier_level(1, sps_max_sub_layers_minus1)
    let profile_space = r.bits(2).ok_or_else(bad)? as u8;
    let tier_flag = r.flag().ok_or_else(bad)?;
    let profile_idc = r.bits(5).ok_or_else(bad)? as u8;
    let profile_compatibility = r.bits(32).ok_or_else(bad)?;
    let hi = u64::from(r.bits(16).ok_or_else(bad)?);
    let lo = u64::from(r.bits(32).ok_or_else(bad)?);
    let constraint_flags = (hi << 32) | lo;
    let level_idc = r.bits(8).ok_or_else(bad)? as u8;
    let mut sub_profile = [false; 8];
    let mut sub_level = [false; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        sub_profile[i] = r.flag().ok_or_else(bad)?;
        sub_level[i] = r.flag().ok_or_else(bad)?;
    }
    if max_sub_layers_minus1 > 0 {
        r.skip(2 * (8 - max_sub_layers_minus1 as usize))
            .ok_or_else(bad)?; // reserved_zero_2bits
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if sub_profile[i] {
            r.skip(88).ok_or_else(bad)?;
        }
        if sub_level[i] {
            r.skip(8).ok_or_else(bad)?;
        }
    }

    r.ue().ok_or_else(bad)?; // sps_seq_parameter_set_id
    let chroma_format_idc = r.ue().ok_or_else(bad)?;
    if chroma_format_idc > 3 {
        return Err(bad());
    }
    if chroma_format_idc == 3 {
        r.bit().ok_or_else(bad)?; // separate_colour_plane_flag
    }
    r.ue().ok_or_else(bad)?; // pic_width_in_luma_samples
    r.ue().ok_or_else(bad)?; // pic_height_in_luma_samples
    if r.flag().ok_or_else(bad)? {
        for _ in 0..4 {
            r.ue().ok_or_else(bad)?; // conformance window offsets
        }
    }
    let luma = r.ue().ok_or_else(bad)?;
    let chroma = r.ue().ok_or_else(bad)?;
    if luma > 8 || chroma > 8 {
        return Err(bad());
    }
    Ok(SpsInfo {
        profile_space,
        tier_flag,
        profile_idc,
        profile_compatibility,
        constraint_flags,
        level_idc,
        chroma_format_idc: chroma_format_idc as u8,
        bit_depth_luma: 8 + luma as u8,
        bit_depth_chroma: 8 + chroma as u8,
        num_temporal_layers: max_sub_layers_minus1 as u8 + 1,
        temporal_id_nested,
    })
}

/// Builds the `HEVCDecoderConfigurationRecord` (payload of the `hvcC` box).
pub(crate) fn build_hvcc(
    vps: &[Vec<u8>],
    sps: &[Vec<u8>],
    pps: &[Vec<u8>],
) -> Result<Vec<u8>, MuxError> {
    if vps.is_empty() {
        return Err(MuxError::MissingParameterSets("HEVC VPS"));
    }
    let first = sps
        .first()
        .ok_or(MuxError::MissingParameterSets("HEVC SPS"))?;
    if pps.is_empty() {
        return Err(MuxError::MissingParameterSets("HEVC PPS"));
    }
    let info = parse_sps(first)?;
    let mut out = Vec::new();
    out.put_u8(1); // configurationVersion
    out.put_u8((info.profile_space << 6) | (u8::from(info.tier_flag) << 5) | info.profile_idc);
    out.put_u32(info.profile_compatibility);
    out.extend_from_slice(&info.constraint_flags.to_be_bytes()[2..]);
    out.put_u8(info.level_idc);
    out.put_u16(0xF000); // reserved + min_spatial_segmentation_idc = 0 (unknown)
    out.put_u8(0xFC); // reserved + parallelismType = 0 (unknown)
    out.put_u8(0xFC | info.chroma_format_idc);
    out.put_u8(0xF8 | (info.bit_depth_luma - 8));
    out.put_u8(0xF8 | (info.bit_depth_chroma - 8));
    out.put_u16(0); // avgFrameRate = unspecified
    out.put_u8(
        // constantFrameRate = 0, numTemporalLayers, temporalIdNested, lengthSizeMinusOne = 3
        (info.num_temporal_layers << 3) | (u8::from(info.temporal_id_nested) << 2) | 3,
    );
    out.put_u8(3); // numOfArrays
    for (nal_type, sets) in [(NAL_VPS, vps), (NAL_SPS, sps), (NAL_PPS, pps)] {
        out.put_u8(0x80 | nal_type); // array_completeness = 1
        out.put_u16(u16::try_from(sets.len()).map_err(|_| MuxError::TooLarge("parameter sets"))?);
        for nal in sets {
            out.put_u16(u16::try_from(nal.len()).map_err(|_| MuxError::TooLarge("parameter set"))?);
            out.extend_from_slice(nal);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BitWriter;

    /// A Main-profile style SPS with optional sub-layer info.
    fn synthetic_sps(
        tier: bool,
        profile: u8,
        chroma: u32,
        luma_minus8: u32,
        sub_layers_minus1: u32,
        conformance: bool,
    ) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.bits(0, 4).bits(u64::from(sub_layers_minus1), 3).bit(1);
        w.bits(0, 2)
            .bit(u32::from(tier))
            .bits(u64::from(profile), 5);
        w.bits(0x6000_0000, 32); // compat flags: profile 1 and 2
        w.bits(0x9000, 16).bits(0, 32); // progressive + frame_only, rest zero
        w.bits(93, 8); // level 3.1
        for i in 0..sub_layers_minus1 {
            w.bit(u32::from(i == 0)).bit(u32::from(i == 0)); // first sub-layer has both
        }
        if sub_layers_minus1 > 0 {
            for _ in sub_layers_minus1..8 {
                w.bits(0, 2);
            }
        }
        for i in 0..sub_layers_minus1 {
            if i == 0 {
                w.bits(0, 32).bits(0, 32).bits(0, 24); // 88 bits profile
                w.bits(90, 8);
            }
        }
        w.ue(0).ue(chroma);
        if chroma == 3 {
            w.bit(0);
        }
        w.ue(320).ue(240);
        if conformance {
            w.bit(1).ue(0).ue(0).ue(0).ue(4);
        } else {
            w.bit(0);
        }
        w.ue(luma_minus8).ue(luma_minus8);
        w.ue(4); // log2_max_pic_order_cnt_lsb_minus4
        let mut nal = vec![NAL_SPS << 1, 1];
        nal.extend(w.finish());
        nal
    }

    #[test]
    fn classifies_nals() {
        assert_eq!(classify(&[0x40, 1]), NalClass::Vps);
        assert_eq!(classify(&[0x42, 1]), NalClass::Sps);
        assert_eq!(classify(&[0x44, 1]), NalClass::Pps);
        assert_eq!(classify(&[0x46, 1]), NalClass::Aud);
        assert_eq!(classify(&[0x4E, 1]), NalClass::Other); // prefix SEI (39)
        assert_eq!(classify(&[0x26, 1]), NalClass::Other); // IDR_W_RADL (19)
        assert_eq!(classify(&[]), NalClass::Other);
    }

    #[test]
    fn parses_main_profile_sps() {
        let info = parse_sps(&synthetic_sps(false, 1, 1, 0, 0, true)).unwrap();
        assert_eq!(info.profile_space, 0);
        assert!(!info.tier_flag);
        assert_eq!(info.profile_idc, 1);
        assert_eq!(info.profile_compatibility, 0x6000_0000);
        assert_eq!(info.constraint_flags, 0x9000_0000_0000);
        assert_eq!(info.level_idc, 93);
        assert_eq!(info.chroma_format_idc, 1);
        assert_eq!((info.bit_depth_luma, info.bit_depth_chroma), (8, 8));
        assert_eq!(info.num_temporal_layers, 1);
        assert!(info.temporal_id_nested);
    }

    #[test]
    fn parses_main10_with_sub_layers() {
        let info = parse_sps(&synthetic_sps(true, 2, 1, 2, 2, false)).unwrap();
        assert!(info.tier_flag);
        assert_eq!(info.profile_idc, 2);
        assert_eq!(info.level_idc, 93);
        assert_eq!((info.bit_depth_luma, info.bit_depth_chroma), (10, 10));
        assert_eq!(info.num_temporal_layers, 3);
    }

    #[test]
    fn parses_444() {
        let info = parse_sps(&synthetic_sps(false, 4, 3, 0, 0, false)).unwrap();
        assert_eq!(info.chroma_format_idc, 3);
    }

    #[test]
    fn sps_with_emulation_prevention_parses() {
        let mut nal = synthetic_sps(false, 1, 1, 0, 0, false);
        // Insert an emulation prevention byte after the zero run in the
        // constraint flags (bytes ... 90 00 | 00 00 00 00 ...).
        let pos = nal.windows(3).position(|w| w == [0x90, 0, 0]).unwrap() + 3;
        nal.insert(pos + 1, 3);
        let plain = parse_sps(&synthetic_sps(false, 1, 1, 0, 0, false)).unwrap();
        assert_eq!(parse_sps(&nal).unwrap(), plain);
    }

    #[test]
    fn rejects_truncated_sps() {
        let sps = synthetic_sps(false, 1, 1, 0, 0, false);
        assert!(parse_sps(&[0x42]).is_err());
        for cut in 3..sps.len() - 3 {
            // Anything cut before the bit depth fields must fail cleanly.
            let _ = parse_sps(&sps[..cut]);
        }
        assert!(parse_sps(&sps[..14]).is_err());
    }

    #[test]
    fn builds_hvcc() {
        let vps = vec![0x40, 0x01, 0x0C, 0x01];
        let sps = synthetic_sps(false, 1, 1, 0, 0, false);
        let pps = vec![0x44, 0x01, 0xC1, 0x72];
        let hvcc = build_hvcc(
            std::slice::from_ref(&vps),
            std::slice::from_ref(&sps),
            std::slice::from_ref(&pps),
        )
        .unwrap();
        let mut expected = vec![1, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 93];
        expected.extend([0xF0, 0x00, 0xFC, 0xFD, 0xF8, 0xF8, 0, 0]);
        expected.push((1 << 3) | (1 << 2) | 3);
        expected.push(3);
        for (t, nal) in [(32u8, &vps), (33, &sps), (34, &pps)] {
            expected.push(0x80 | t);
            expected.extend([0, 1]);
            expected.extend((nal.len() as u16).to_be_bytes());
            expected.extend(nal.iter());
        }
        assert_eq!(hvcc, expected);
    }

    #[test]
    fn hvcc_requires_all_parameter_sets() {
        let sps = synthetic_sps(false, 1, 1, 0, 0, false);
        let one = vec![vec![0x40, 1]];
        let sps_v = vec![sps];
        assert!(matches!(
            build_hvcc(&[], &sps_v, &one),
            Err(MuxError::MissingParameterSets("HEVC VPS"))
        ));
        assert!(matches!(
            build_hvcc(&one, &[], &one),
            Err(MuxError::MissingParameterSets("HEVC SPS"))
        ));
        assert!(matches!(
            build_hvcc(&one, &sps_v, &[]),
            Err(MuxError::MissingParameterSets("HEVC PPS"))
        ));
    }
}
