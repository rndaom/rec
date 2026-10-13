//! H.264: NAL classification and `avcC` construction.

use crate::bits::{BitReader, unescape_rbsp};
use crate::boxes::Put;
use crate::error::MuxError;
use crate::video::NalClass;

pub(crate) fn classify(nal: &[u8]) -> NalClass {
    match nal.first().map(|b| b & 0x1F) {
        Some(7) => NalClass::Sps,
        Some(8) => NalClass::Pps,
        Some(9) => NalClass::Aud,
        _ => NalClass::Other,
    }
}

/// Fields of the SPS needed for `avcC`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SpsInfo {
    pub profile_idc: u8,
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub chroma_format_idc: u8,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
}

/// Profiles whose SPS carries chroma format and bit depth.
fn has_chroma_info(profile_idc: u8) -> bool {
    matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    )
}

pub(crate) fn parse_sps(nal: &[u8]) -> Result<SpsInfo, MuxError> {
    const BAD: &str = "truncated or invalid H.264 SPS";
    let bad = || MuxError::malformed("H.264 SPS", BAD);
    if nal.len() < 4 {
        return Err(bad());
    }
    let rbsp = unescape_rbsp(&nal[1..]);
    let mut r = BitReader::new(&rbsp);
    let profile_idc = r.bits(8).ok_or_else(bad)? as u8;
    let constraint_flags = r.bits(8).ok_or_else(bad)? as u8;
    let level_idc = r.bits(8).ok_or_else(bad)? as u8;
    r.ue().ok_or_else(bad)?; // seq_parameter_set_id
    let (mut chroma_format_idc, mut luma, mut chroma) = (1u32, 0u32, 0u32);
    if has_chroma_info(profile_idc) {
        chroma_format_idc = r.ue().ok_or_else(bad)?;
        if chroma_format_idc == 3 {
            r.bit().ok_or_else(bad)?; // separate_colour_plane_flag
        }
        luma = r.ue().ok_or_else(bad)?;
        chroma = r.ue().ok_or_else(bad)?;
    }
    if chroma_format_idc > 3 || luma > 6 || chroma > 6 {
        return Err(bad());
    }
    Ok(SpsInfo {
        profile_idc,
        constraint_flags,
        level_idc,
        chroma_format_idc: chroma_format_idc as u8,
        bit_depth_luma: 8 + luma as u8,
        bit_depth_chroma: 8 + chroma as u8,
    })
}

/// Builds the `AVCDecoderConfigurationRecord` (payload of the `avcC` box).
pub(crate) fn build_avcc(sps: &[Vec<u8>], pps: &[Vec<u8>]) -> Result<Vec<u8>, MuxError> {
    let first = sps
        .first()
        .ok_or(MuxError::MissingParameterSets("H.264 SPS"))?;
    if pps.is_empty() {
        return Err(MuxError::MissingParameterSets("H.264 PPS"));
    }
    if sps.len() > 31 || pps.len() > 255 {
        return Err(MuxError::TooLarge("number of parameter sets"));
    }
    let info = parse_sps(first)?;
    let mut out = Vec::new();
    out.put_u8(1); // configurationVersion
    out.put_u8(info.profile_idc);
    out.put_u8(info.constraint_flags);
    out.put_u8(info.level_idc);
    out.put_u8(0xFC | 3); // reserved + lengthSizeMinusOne
    out.put_u8(0xE0 | sps.len() as u8);
    for s in sps {
        put_sized(&mut out, s)?;
    }
    out.put_u8(pps.len() as u8);
    for p in pps {
        put_sized(&mut out, p)?;
    }
    if !matches!(info.profile_idc, 66 | 77 | 88) {
        out.put_u8(0xFC | info.chroma_format_idc);
        out.put_u8(0xF8 | (info.bit_depth_luma - 8));
        out.put_u8(0xF8 | (info.bit_depth_chroma - 8));
        out.put_u8(0); // numOfSequenceParameterSetExt
    }
    Ok(out)
}

fn put_sized(out: &mut Vec<u8>, nal: &[u8]) -> Result<(), MuxError> {
    let len = u16::try_from(nal.len()).map_err(|_| MuxError::TooLarge("parameter set"))?;
    out.put_u16(len);
    out.extend_from_slice(nal);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BitWriter;

    /// Builds a minimal SPS NAL (header plus the fields up to the picture
    /// order count) for the given profile.
    fn synthetic_sps(profile: u8, level: u8, chroma: u32, depth_minus8: u32) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.bits(u64::from(profile), 8)
            .bits(0x0C, 8)
            .bits(u64::from(level), 8);
        w.ue(0); // sps id
        if has_chroma_info(profile) {
            w.ue(chroma);
            if chroma == 3 {
                w.bit(0);
            }
            w.ue(depth_minus8).ue(depth_minus8);
        }
        w.ue(0).ue(0).ue(4);
        let mut nal = vec![0x67];
        nal.extend(w.finish());
        nal
    }

    #[test]
    fn classifies_nals() {
        assert_eq!(classify(&[0x67, 1]), NalClass::Sps);
        assert_eq!(classify(&[0x68]), NalClass::Pps);
        assert_eq!(classify(&[0x09, 0x10]), NalClass::Aud);
        assert_eq!(classify(&[0x65]), NalClass::Other); // IDR slice
        assert_eq!(classify(&[0x06]), NalClass::Other); // SEI is kept
        assert_eq!(classify(&[]), NalClass::Other);
    }

    #[test]
    fn parses_high_profile_sps() {
        let info = parse_sps(&synthetic_sps(100, 31, 1, 0)).unwrap();
        assert_eq!(info.profile_idc, 100);
        assert_eq!(info.constraint_flags, 0x0C);
        assert_eq!(info.level_idc, 31);
        assert_eq!(info.chroma_format_idc, 1);
        assert_eq!((info.bit_depth_luma, info.bit_depth_chroma), (8, 8));
    }

    #[test]
    fn parses_high_444_10_bit_sps() {
        let info = parse_sps(&synthetic_sps(244, 40, 3, 2)).unwrap();
        assert_eq!(info.chroma_format_idc, 3);
        assert_eq!((info.bit_depth_luma, info.bit_depth_chroma), (10, 10));
    }

    #[test]
    fn baseline_has_implied_420_8bit() {
        let info = parse_sps(&synthetic_sps(66, 30, 0, 0)).unwrap();
        assert_eq!(info.chroma_format_idc, 1);
        assert_eq!(info.bit_depth_luma, 8);
    }

    #[test]
    fn sps_with_emulation_prevention_still_parses() {
        let mut w = BitWriter::new();
        w.bits(100, 8).bits(0, 8).bits(0, 8).ue(0).ue(1).ue(0).ue(0);
        let rbsp = w.finish();
        // Constraint flags and level are both zero, so 00 00 may be followed
        // by an emulation prevention byte.
        let mut escaped = vec![0x67, 100, 0, 0, 3];
        escaped.extend_from_slice(&rbsp[3..]);
        let info = parse_sps(&escaped).unwrap();
        assert_eq!(info.profile_idc, 100);
        assert_eq!(info.level_idc, 0);
        assert_eq!(info.chroma_format_idc, 1);
    }

    #[test]
    fn rejects_truncated_sps() {
        assert!(parse_sps(&[0x67, 100]).is_err());
        assert!(parse_sps(&[0x67, 100, 0, 30]).is_err()); // no sps id
        assert!(parse_sps(&[0x67, 100, 0, 30, 0x00, 0x00]).is_err());
    }

    #[test]
    fn builds_avcc_with_high_profile_extension() {
        let sps = synthetic_sps(100, 31, 1, 0);
        let pps = vec![0x68, 0xEE, 0x3C, 0x80];
        let avcc = build_avcc(std::slice::from_ref(&sps), std::slice::from_ref(&pps)).unwrap();
        let mut expected = vec![1, 100, 0x0C, 31, 0xFF, 0xE1];
        expected.extend((sps.len() as u16).to_be_bytes());
        expected.extend(&sps);
        expected.push(1);
        expected.extend((pps.len() as u16).to_be_bytes());
        expected.extend(&pps);
        expected.extend([0xFD, 0xF8, 0xF8, 0]);
        assert_eq!(avcc, expected);
    }

    #[test]
    fn avcc_for_main_profile_has_no_extension() {
        let sps = synthetic_sps(77, 40, 0, 0);
        let avcc = build_avcc(std::slice::from_ref(&sps), &[vec![0x68, 1]]).unwrap();
        assert_eq!(avcc.len(), 6 + 2 + sps.len() + 1 + 2 + 2);
        assert_eq!(&avcc[..4], [1, 77, 0x0C, 40]);
    }

    #[test]
    fn avcc_requires_sps_and_pps() {
        let sps = synthetic_sps(100, 31, 1, 0);
        assert!(matches!(
            build_avcc(&[], &[vec![0x68]]),
            Err(MuxError::MissingParameterSets(_))
        ));
        assert!(matches!(
            build_avcc(&[sps], &[]),
            Err(MuxError::MissingParameterSets(_))
        ));
    }
}
