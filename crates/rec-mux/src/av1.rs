//! AV1: OBU stream conversion and `av1C` construction.

use crate::bits::BitReader;
use crate::boxes::Put;
use crate::error::MuxError;

const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;
const OBU_FRAME_HEADER: u8 = 3;
const OBU_FRAME: u8 = 6;
const OBU_PADDING: u8 = 15;

/// One OBU split out of a temporal unit.
#[derive(Clone, Copy, Debug)]
struct Obu<'a> {
    kind: u8,
    /// Header byte and optional extension byte.
    header: &'a [u8],
    payload: &'a [u8],
    /// The OBU as it appeared in the stream, including its size field.
    raw: &'a [u8],
    has_size_field: bool,
}

fn read_leb128(data: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (i, &b) in data.iter().take(8).enumerate() {
        value |= u64::from(b & 0x7F) << (7 * i);
        if b & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

fn put_leb128(out: &mut Vec<u8>, mut value: usize) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn split_obus(data: &[u8]) -> Result<Vec<Obu<'_>>, MuxError> {
    const BAD: &str = "invalid AV1 OBU stream";
    let bad = || MuxError::malformed("video packet", BAD);
    let mut obus = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let first = data[pos];
        if first & 0x80 != 0 {
            return Err(bad()); // obu_forbidden_bit
        }
        let kind = (first >> 3) & 0x0F;
        let has_extension = first & 0x04 != 0;
        let has_size_field = first & 0x02 != 0;
        let header_len = 1 + usize::from(has_extension);
        let header = data.get(pos..pos + header_len).ok_or_else(bad)?;
        let mut cursor = pos + header_len;
        let payload_len = if has_size_field {
            let (len, used) = read_leb128(&data[cursor..]).ok_or_else(bad)?;
            cursor += used;
            usize::try_from(len).map_err(|_| bad())?
        } else {
            data.len() - cursor
        };
        let end = cursor.checked_add(payload_len).ok_or_else(bad)?;
        let payload = data.get(cursor..end).ok_or_else(bad)?;
        obus.push(Obu {
            kind,
            header,
            payload,
            raw: &data[pos..end],
            has_size_field,
        });
        pos = end;
    }
    Ok(obus)
}

/// A temporal unit converted to an MP4 sample.
#[derive(Debug)]
pub(crate) struct Converted {
    /// OBUs with size fields; temporal delimiters and padding removed.
    pub sample: Vec<u8>,
    /// Sequence header OBUs (normalised to carry a size field).
    pub sequence_headers: Vec<Vec<u8>>,
}

/// Converts a low-overhead OBU temporal unit to an MP4 sample.
pub(crate) fn convert_temporal_unit(data: &[u8]) -> Result<Converted, MuxError> {
    let obus = split_obus(data)?;
    let mut sample = Vec::with_capacity(data.len());
    let mut sequence_headers: Vec<Vec<u8>> = Vec::new();
    let mut has_picture = false;
    for obu in obus {
        match obu.kind {
            OBU_TEMPORAL_DELIMITER | OBU_PADDING => continue,
            OBU_FRAME | OBU_FRAME_HEADER => has_picture = true,
            _ => {}
        }
        if obu.kind == OBU_SEQUENCE_HEADER {
            let mut normalised = Vec::with_capacity(obu.payload.len() + 4);
            push_with_size(&mut normalised, &obu);
            if !sequence_headers.contains(&normalised) {
                sequence_headers.push(normalised);
            }
        }
        if obu.has_size_field {
            sample.extend_from_slice(obu.raw);
        } else {
            push_with_size(&mut sample, &obu);
        }
    }
    if !has_picture {
        return Err(MuxError::malformed(
            "video packet",
            "temporal unit contains no frame OBU",
        ));
    }
    Ok(Converted {
        sample,
        sequence_headers,
    })
}

fn push_with_size(out: &mut Vec<u8>, obu: &Obu<'_>) {
    out.push(obu.header[0] | 0x02);
    out.extend_from_slice(&obu.header[1..]);
    put_leb128(out, obu.payload.len());
    out.extend_from_slice(obu.payload);
}

/// Fields of the sequence header needed for `av1C`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SequenceInfo {
    pub profile: u8,
    pub level_idx: u8,
    pub tier: u8,
    pub high_bitdepth: bool,
    pub twelve_bit: bool,
    pub monochrome: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    pub chroma_sample_position: u8,
}

/// Parses a sequence header OBU (with size field) up to the end of
/// `color_config()`.
pub(crate) fn parse_sequence_header(obu: &[u8]) -> Result<SequenceInfo, MuxError> {
    const BAD: &str = "truncated or invalid AV1 sequence header";
    let bad = || MuxError::malformed("AV1 sequence header", BAD);
    let obus = split_obus(obu)?;
    let [first] = obus.as_slice() else {
        return Err(bad());
    };
    if first.kind != OBU_SEQUENCE_HEADER {
        return Err(bad());
    }
    let mut r = BitReader::new(first.payload);
    let profile = r.bits(3).ok_or_else(bad)? as u8;
    if profile > 2 {
        return Err(bad());
    }
    r.bit().ok_or_else(bad)?; // still_picture
    let reduced = r.flag().ok_or_else(bad)?;
    let (level_idx, tier);
    if reduced {
        level_idx = r.bits(5).ok_or_else(bad)? as u8;
        tier = 0;
    } else {
        let timing_info_present = r.flag().ok_or_else(bad)?;
        let mut decoder_model_info_present = false;
        let mut buffer_delay_length = 0;
        if timing_info_present {
            r.skip(64).ok_or_else(bad)?; // num_units_in_display_tick, time_scale
            if r.flag().ok_or_else(bad)? {
                r.uvlc().ok_or_else(bad)?; // num_ticks_per_picture_minus_1
            }
            decoder_model_info_present = r.flag().ok_or_else(bad)?;
            if decoder_model_info_present {
                buffer_delay_length = r.bits(5).ok_or_else(bad)? as usize + 1;
                r.skip(32).ok_or_else(bad)?; // num_units_in_decoding_tick
                r.skip(10).ok_or_else(bad)?; // removal / presentation time lengths
            }
        }
        let initial_display_delay_present = r.flag().ok_or_else(bad)?;
        let operating_points = r.bits(5).ok_or_else(bad)? + 1;
        let mut first_level = 0;
        let mut first_tier = 0;
        for i in 0..operating_points {
            r.skip(12).ok_or_else(bad)?; // operating_point_idc
            let level = r.bits(5).ok_or_else(bad)? as u8;
            let t = if level > 7 {
                r.bits(1).ok_or_else(bad)? as u8
            } else {
                0
            };
            if i == 0 {
                first_level = level;
                first_tier = t;
            }
            if decoder_model_info_present && r.flag().ok_or_else(bad)? {
                r.skip(2 * buffer_delay_length + 1).ok_or_else(bad)?;
            }
            if initial_display_delay_present && r.flag().ok_or_else(bad)? {
                r.skip(4).ok_or_else(bad)?;
            }
        }
        level_idx = first_level;
        tier = first_tier;
    }

    let width_bits = r.bits(4).ok_or_else(bad)? as usize + 1;
    let height_bits = r.bits(4).ok_or_else(bad)? as usize + 1;
    r.skip(width_bits + height_bits).ok_or_else(bad)?; // max frame size
    if !reduced && r.flag().ok_or_else(bad)? {
        r.skip(7).ok_or_else(bad)?; // frame id lengths
    }
    r.skip(3).ok_or_else(bad)?; // 128x128 superblock, filter intra, intra edge
    if !reduced {
        r.skip(4).ok_or_else(bad)?; // interintra, masked, warped, dual filter
        let order_hint = r.flag().ok_or_else(bad)?;
        if order_hint {
            r.skip(2).ok_or_else(bad)?; // jnt_comp, ref_frame_mvs
        }
        let choose_screen_content_tools = r.flag().ok_or_else(bad)?;
        let force_screen_content_tools = if choose_screen_content_tools {
            true // SELECT_SCREEN_CONTENT_TOOLS
        } else {
            r.flag().ok_or_else(bad)?
        };
        if force_screen_content_tools && !r.flag().ok_or_else(bad)? {
            r.skip(1).ok_or_else(bad)?; // seq_force_integer_mv
        }
        if order_hint {
            r.skip(3).ok_or_else(bad)?; // order_hint_bits_minus_1
        }
    }
    r.skip(3).ok_or_else(bad)?; // superres, cdef, restoration

    // color_config()
    let high_bitdepth = r.flag().ok_or_else(bad)?;
    let twelve_bit = profile == 2 && high_bitdepth && r.flag().ok_or_else(bad)?;
    let monochrome = profile != 1 && r.flag().ok_or_else(bad)?;
    let (mut primaries, mut transfer, mut matrix) = (2, 2, 2);
    if r.flag().ok_or_else(bad)? {
        primaries = r.bits(8).ok_or_else(bad)?;
        transfer = r.bits(8).ok_or_else(bad)?;
        matrix = r.bits(8).ok_or_else(bad)?;
    }
    let (sx, sy, position);
    if monochrome {
        r.skip(1).ok_or_else(bad)?; // color_range
        sx = true;
        sy = true;
        position = 0;
    } else if primaries == 1 && transfer == 13 && matrix == 0 {
        // sRGB with identity matrix: full range 4:4:4
        sx = false;
        sy = false;
        position = 0;
    } else {
        r.skip(1).ok_or_else(bad)?; // color_range
        match profile {
            0 => {
                sx = true;
                sy = true;
            }
            1 => {
                sx = false;
                sy = false;
            }
            _ => {
                if twelve_bit {
                    sx = r.flag().ok_or_else(bad)?;
                    sy = if sx { r.flag().ok_or_else(bad)? } else { false };
                } else {
                    sx = true;
                    sy = false;
                }
            }
        }
        position = if sx && sy {
            r.bits(2).ok_or_else(bad)? as u8
        } else {
            0
        };
    }
    Ok(SequenceInfo {
        profile,
        level_idx,
        tier,
        high_bitdepth,
        twelve_bit,
        monochrome,
        subsampling_x: sx,
        subsampling_y: sy,
        chroma_sample_position: position,
    })
}

/// Builds the `AV1CodecConfigurationRecord` (payload of the `av1C` box) from
/// the sequence header OBU(s) of the first keyframe.
pub(crate) fn build_av1c(sequence_headers: &[Vec<u8>]) -> Result<Vec<u8>, MuxError> {
    let first = sequence_headers
        .first()
        .ok_or(MuxError::MissingParameterSets("AV1 sequence header"))?;
    let info = parse_sequence_header(first)?;
    let mut out = Vec::with_capacity(4 + first.len());
    out.put_u8(0x81); // marker + version 1
    out.put_u8((info.profile << 5) | info.level_idx);
    out.put_u8(
        (info.tier << 7)
            | (u8::from(info.high_bitdepth) << 6)
            | (u8::from(info.twelve_bit) << 5)
            | (u8::from(info.monochrome) << 4)
            | (u8::from(info.subsampling_x) << 3)
            | (u8::from(info.subsampling_y) << 2)
            | info.chroma_sample_position,
    );
    out.put_u8(0); // no initial_presentation_delay
    out.extend_from_slice(first);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BitWriter;

    /// Wraps a payload in a sequence header OBU with a size field.
    fn obu(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![(kind << 3) | 0x02];
        put_leb128(&mut out, payload.len());
        out.extend_from_slice(payload);
        out
    }

    struct Opts {
        profile: u32,
        level: u32,
        high_bitdepth: bool,
        twelve_bit: bool,
        mono: bool,
        color_description: Option<(u32, u32, u32)>,
        timing: bool,
        reduced: bool,
        ops: u32,
    }

    impl Default for Opts {
        fn default() -> Self {
            Self {
                profile: 0,
                level: 4,
                high_bitdepth: false,
                twelve_bit: false,
                mono: false,
                color_description: None,
                timing: false,
                reduced: false,
                ops: 1,
            }
        }
    }

    fn sequence_header(o: &Opts) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.bits(u64::from(o.profile), 3);
        w.bit(0); // still_picture
        w.bit(u32::from(o.reduced));
        if o.reduced {
            w.bits(u64::from(o.level), 5);
        } else {
            w.bit(u32::from(o.timing));
            if o.timing {
                w.bits(1, 32).bits(30, 32);
                w.bit(1).ue(0); // equal_picture_interval, uvlc(0) == "1"
                w.bit(1); // decoder_model_info_present
                w.bits(9, 5).bits(1, 32).bits(0, 10); // 10 bit delays
            }
            w.bit(1); // initial_display_delay_present
            w.bits(u64::from(o.ops - 1), 5);
            for i in 0..o.ops {
                w.bits(0, 12);
                let level = if i == 0 { o.level } else { 9 };
                w.bits(u64::from(level), 5);
                if level > 7 {
                    w.bit(1); // tier
                }
                if o.timing {
                    w.bit(1); // decoder_model_present_for_this_op
                    w.bits(0, 21); // 10 + 10 + 1
                }
                w.bit(1).bits(3, 4); // initial_display_delay
            }
        }
        w.bits(10, 4).bits(10, 4).bits(319, 11).bits(239, 11);
        if !o.reduced {
            w.bit(0); // frame_id_numbers_present
        }
        w.bits(0, 3);
        if !o.reduced {
            w.bits(0, 4);
            w.bit(1).bits(0, 2); // order hint + two flags
            w.bit(1); // seq_choose_screen_content_tools
            w.bit(1); // seq_choose_integer_mv
            w.bits(6, 3);
        }
        w.bits(0, 3);
        // color_config
        w.bit(u32::from(o.high_bitdepth));
        if o.profile == 2 && o.high_bitdepth {
            w.bit(u32::from(o.twelve_bit));
        }
        if o.profile != 1 {
            w.bit(u32::from(o.mono));
        }
        match o.color_description {
            Some((p, t, m)) => {
                w.bit(1)
                    .bits(u64::from(p), 8)
                    .bits(u64::from(t), 8)
                    .bits(u64::from(m), 8);
            }
            None => {
                w.bit(0);
            }
        }
        if o.mono {
            w.bit(0);
        } else if o.color_description == Some((1, 13, 0)) {
            // nothing more
        } else {
            w.bit(0); // color_range
            if o.profile == 0 {
                w.bits(0, 2); // chroma_sample_position
            } else if o.profile == 2 && o.twelve_bit {
                w.bit(1).bit(0); // 4:2:2
            }
        }
        w.bit(0); // separate_uv_delta_q
        obu(OBU_SEQUENCE_HEADER, &w.finish())
    }

    #[test]
    fn parses_main_8bit_420() {
        let info = parse_sequence_header(&sequence_header(&Opts::default())).unwrap();
        assert_eq!(info.profile, 0);
        assert_eq!(info.level_idx, 4);
        assert_eq!(info.tier, 0);
        assert!(!info.high_bitdepth && !info.twelve_bit && !info.monochrome);
        assert!(info.subsampling_x && info.subsampling_y);
        assert_eq!(info.chroma_sample_position, 0);
    }

    #[test]
    fn parses_10bit_with_timing_and_decoder_model_and_two_operating_points() {
        let o = Opts {
            high_bitdepth: true,
            level: 12,
            timing: true,
            ops: 2,
            ..Opts::default()
        };
        let info = parse_sequence_header(&sequence_header(&o)).unwrap();
        assert!(info.high_bitdepth && !info.twelve_bit);
        assert_eq!(info.level_idx, 12);
        assert_eq!(info.tier, 1);
        assert!(info.subsampling_x && info.subsampling_y);
    }

    #[test]
    fn parses_monochrome_and_reduced_headers() {
        let info = parse_sequence_header(&sequence_header(&Opts {
            mono: true,
            ..Opts::default()
        }))
        .unwrap();
        assert!(info.monochrome && info.subsampling_x && info.subsampling_y);

        let info = parse_sequence_header(&sequence_header(&Opts {
            reduced: true,
            level: 8,
            ..Opts::default()
        }))
        .unwrap();
        assert_eq!(info.level_idx, 8);
        assert!(!info.monochrome);
    }

    #[test]
    fn parses_444_srgb_and_12bit_422() {
        let info = parse_sequence_header(&sequence_header(&Opts {
            profile: 1,
            color_description: Some((1, 13, 0)),
            ..Opts::default()
        }))
        .unwrap();
        assert!(!info.subsampling_x && !info.subsampling_y);

        let info = parse_sequence_header(&sequence_header(&Opts {
            profile: 2,
            high_bitdepth: true,
            twelve_bit: true,
            ..Opts::default()
        }))
        .unwrap();
        assert!(info.twelve_bit);
        assert!(info.subsampling_x && !info.subsampling_y);
    }

    #[test]
    fn rejects_truncated_and_wrong_obus() {
        let good = sequence_header(&Opts::default());
        assert!(parse_sequence_header(&good[..good.len() - 3]).is_err());
        assert!(parse_sequence_header(&obu(OBU_FRAME, &[1, 2, 3])).is_err());
        assert!(parse_sequence_header(&[]).is_err());
        let mut two = good.clone();
        two.extend_from_slice(&good);
        assert!(parse_sequence_header(&two).is_err());
    }

    #[test]
    fn builds_av1c() {
        let seq = sequence_header(&Opts {
            level: 8,
            high_bitdepth: true,
            ..Opts::default()
        });
        let av1c = build_av1c(std::slice::from_ref(&seq)).unwrap();
        assert_eq!(av1c[0], 0x81);
        assert_eq!(av1c[1], 8); // profile 0, level 8
        // tier 1 (level > 7), high_bitdepth, 4:2:0, position 0
        assert_eq!(av1c[2], 0b1100_1100);
        assert_eq!(av1c[3], 0);
        assert_eq!(&av1c[4..], &seq[..]);
        assert!(matches!(
            build_av1c(&[]),
            Err(MuxError::MissingParameterSets(_))
        ));
    }

    #[test]
    fn converts_temporal_units() {
        let seq = sequence_header(&Opts::default());
        let mut tu = vec![0x12, 0x00]; // temporal delimiter with size field
        tu.extend_from_slice(&seq);
        tu.extend_from_slice(&obu(15, &[0, 0])); // padding
        let frame = obu(OBU_FRAME, &[9, 8, 7]);
        tu.extend_from_slice(&frame);
        let c = convert_temporal_unit(&tu).unwrap();
        assert_eq!(c.sequence_headers, std::slice::from_ref(&seq));
        let mut expected = seq;
        expected.extend_from_slice(&frame);
        assert_eq!(c.sample, expected);
    }

    #[test]
    fn last_obu_without_size_field_gets_one() {
        let mut tu = vec![0x12, 0x00]; // temporal delimiter with size field
        tu.push(OBU_FRAME << 3); // frame OBU without size field runs to the end
        tu.extend_from_slice(&[1, 2, 3, 4]);
        let c = convert_temporal_unit(&tu).unwrap();
        assert_eq!(c.sample, [(OBU_FRAME << 3) | 0x02, 4, 1, 2, 3, 4]);
        assert!(c.sequence_headers.is_empty());
    }

    #[test]
    fn rejects_malformed_streams() {
        assert!(convert_temporal_unit(&[]).is_err()); // nothing but no picture
        assert!(convert_temporal_unit(&[0x80, 0]).is_err()); // forbidden bit
        assert!(convert_temporal_unit(&[0x32, 0x05, 1]).is_err()); // size past end
        assert!(convert_temporal_unit(&[0x12, 0x00]).is_err()); // only a delimiter
        assert!(convert_temporal_unit(&[0x32, 0xFF]).is_err()); // unterminated leb128
    }

    #[test]
    fn leb128_roundtrip() {
        for v in [0usize, 1, 127, 128, 300, 16_383, 16_384, 1 << 28] {
            let mut b = Vec::new();
            put_leb128(&mut b, v);
            assert_eq!(read_leb128(&b), Some((v as u64, b.len())));
        }
    }
}
