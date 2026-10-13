//! HEVC HDR10 static metadata as an in-band prefix SEI NAL unit.
//!
//! The packet contract (`rec_core::media`) requires every keyframe of an HDR10
//! recording to carry `mastering_display_colour_volume` and
//! `content_light_level_info`, so a clip cut at any keyframe (or remuxed by
//! another tool) keeps its HDR signalling. Hardware encoders do not write
//! them, so the pipeline builds the NAL once per session with
//! [`hevc_hdr10_sei`] and puts it into each keyframe with
//! [`insert_hevc_hdr10_sei`].
//!
//! Both messages live in one prefix SEI NAL (type 39) placed right before the
//! first slice, after the AUD and parameter sets, which is where HEVC
//! requires prefix SEI.

use std::borrow::Cow;

use rec_core::media::{HdrMetadata, VideoCodec};

use crate::annexb::{self, NalIter};

/// NAL header of a prefix SEI: type 39, `nuh_layer_id` 0,
/// `nuh_temporal_id_plus1` 1.
const PREFIX_SEI_HEADER: [u8; 2] = [39 << 1, 0x01];
const PREFIX_SEI: u8 = 39;
/// SEI `payloadType` of `mastering_display_colour_volume`.
const MDCV: usize = 137;
/// SEI `payloadType` of `content_light_level_info`.
const CLL: usize = 144;
const MDCV_SIZE: u8 = 24;
const CLL_SIZE: u8 = 4;
/// `rbsp_trailing_bits` of a byte-aligned payload: a stop bit and padding.
const RBSP_TRAILING_BITS: u8 = 0x80;

/// The prefix SEI NAL unit (4-byte start code included) carrying `meta` as
/// `mastering_display_colour_volume` followed by `content_light_level_info`.
///
/// The display primaries go out in the order green, blue, red that HEVC
/// prescribes (HdrMetadata keeps the more common red, green, blue); every
/// unit matches [`HdrMetadata`] directly.
pub fn hevc_hdr10_sei(meta: &HdrMetadata) -> Vec<u8> {
    let mut rbsp = Vec::with_capacity(2 + 24 + 2 + 4 + 1);
    rbsp.extend([MDCV as u8, MDCV_SIZE]);
    for [x, y] in [meta.green, meta.blue, meta.red, meta.white] {
        rbsp.extend(x.to_be_bytes());
        rbsp.extend(y.to_be_bytes());
    }
    rbsp.extend(meta.max_luminance.to_be_bytes());
    rbsp.extend(meta.min_luminance.to_be_bytes());
    rbsp.extend([CLL as u8, CLL_SIZE]);
    rbsp.extend(meta.max_cll.to_be_bytes());
    rbsp.extend(meta.max_fall.to_be_bytes());
    rbsp.push(RBSP_TRAILING_BITS);

    let mut nal = Vec::with_capacity(4 + 2 + rbsp.len() + 8);
    nal.extend([0, 0, 0, 1]);
    nal.extend(PREFIX_SEI_HEADER);
    escape_rbsp(&mut nal, &rbsp);
    nal
}

/// Appends `rbsp` to `out` with emulation prevention: a `03` goes between two
/// zero bytes and any following byte `<= 3`, so the payload never looks like
/// a start code. A NAL unit may not end in a zero byte either, so a final
/// `00` gets a `03` after it.
fn escape_rbsp(out: &mut Vec<u8>, rbsp: &[u8]) {
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    if zeros > 0 {
        out.push(3);
    }
}

/// Reads an RBSP byte by byte straight from the escaped NAL body, dropping
/// emulation-prevention bytes on the fly (no copy).
struct Unescaped<'a> {
    data: &'a [u8],
    zeros: u32,
}

impl Iterator for Unescaped<'_> {
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        loop {
            let (&b, rest) = self.data.split_first()?;
            self.data = rest;
            if self.zeros >= 2 && b == 3 {
                self.zeros = 0;
                continue;
            }
            self.zeros = if b == 0 { self.zeros + 1 } else { 0 };
            return Some(b);
        }
    }
}

impl Unescaped<'_> {
    /// Drops `n` RBSP bytes; `None` if fewer remain.
    fn discard(&mut self, n: usize) -> Option<()> {
        (0..n).try_for_each(|_| self.next().map(drop))
    }

    /// An SEI `payloadType` / `payloadSize`: any number of `0xFF` bytes plus
    /// a final byte, summed.
    fn sei_value(&mut self) -> Option<usize> {
        let mut value = 0usize;
        loop {
            let b = self.next()?;
            value = value.saturating_add(usize::from(b));
            if b != 0xFF {
                return Some(value);
            }
        }
    }
}

/// True if the SEI NAL `body` (the bytes after the 2-byte NAL header) holds a
/// `mastering_display_colour_volume` message. Truncated SEI counts as no.
fn sei_has_mdcv(body: &[u8]) -> bool {
    let mut rbsp = Unescaped {
        data: body,
        zeros: 0,
    };
    // The trailing bits (0x80) parse as a payload type with no size, which
    // ends the loop through the `?`.
    while let (Some(kind), Some(size)) = (rbsp.sei_value(), rbsp.sei_value()) {
        if kind == MDCV {
            return true;
        }
        if rbsp.discard(size).is_none() {
            return false;
        }
    }
    false
}

/// What a scan of the NAL units up to the first slice found.
struct Scan {
    has_mdcv: bool,
    /// Offset of the start code of the first VCL NAL unit.
    first_vcl: Option<usize>,
}

fn scan(au: &[u8]) -> Scan {
    let mut has_mdcv = false;
    for nal in NalIter::new(au) {
        let body = &au[nal.payload..nal.end];
        match annexb::nal_type(VideoCodec::Hevc, body) {
            Some(t) if annexb::is_vcl(VideoCodec::Hevc, t) => {
                return Scan {
                    has_mdcv,
                    first_vcl: Some(nal.start),
                };
            }
            Some(PREFIX_SEI) => has_mdcv |= body.get(2..).is_some_and(sei_has_mdcv),
            _ => {}
        }
    }
    Scan {
        has_mdcv,
        first_vcl: None,
    }
}

/// True if the Annex-B HEVC access unit `au` already has a prefix SEI NAL
/// with a `mastering_display_colour_volume` message before its first slice.
pub fn hevc_has_mdcv_sei(au: &[u8]) -> bool {
    scan(au).has_mdcv
}

/// Inserts `sei_nal` (from [`hevc_hdr10_sei`]) into the HEVC access unit `au`
/// right before its first slice NAL, after any AUD, parameter sets and
/// existing prefix SEI. The access unit is copied once.
///
/// `au` is returned borrowed and unchanged if it already carries a
/// `mastering_display_colour_volume` SEI (so applying this twice never
/// duplicates it) or has no slice to put the SEI in front of.
pub fn insert_hevc_hdr10_sei<'a>(au: &'a [u8], sei_nal: &[u8]) -> Cow<'a, [u8]> {
    match scan(au) {
        Scan {
            has_mdcv: false,
            first_vcl: Some(at),
        } => Cow::Owned(annexb::splice(au, at, sei_nal)),
        _ => Cow::Borrowed(au),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> HdrMetadata {
        HdrMetadata {
            red: [35_400, 14_600],
            green: [8_500, 39_850],
            blue: [6_550, 2_300],
            white: [15_635, 16_450],
            max_luminance: 10_000_000,
            min_luminance: 1,
            max_cll: 1000,
            max_fall: 400,
        }
    }

    // HEVC NAL units: header byte = type << 1, then 0x01.
    const AUD: &[u8] = &[0, 0, 0, 1, 0x46, 0x01, 0x50];
    const VPS: &[u8] = &[0, 0, 0, 1, 0x40, 0x01, 0xAA];
    const SPS: &[u8] = &[0, 0, 0, 1, 0x42, 0x01, 0xBB];
    const PPS: &[u8] = &[0, 0, 0, 1, 0x44, 0x01, 0xCC];
    const IDR: &[u8] = &[0, 0, 0, 1, 0x26, 0x01, 0xDD, 0xEE];
    const IDR_3: &[u8] = &[0, 0, 1, 0x26, 0x01, 0xDD, 0xEE];
    const TRAIL: &[u8] = &[0, 0, 0, 1, 0x02, 0x01, 0x11];
    /// Prefix SEI with a user_data_unregistered-like message (type 5, size 2).
    const SEI_OTHER: &[u8] = &[0, 0, 0, 1, 0x4E, 0x01, 0x05, 0x02, 0x12, 0x34, 0x80];
    /// Suffix SEI (type 40) that happens to contain payload type 137.
    const SEI_SUFFIX_137: &[u8] = &[0, 0, 0, 1, 0x50, 0x01, 0x89, 0x01, 0x00, 0x80];

    fn types(data: &[u8]) -> Vec<u8> {
        annexb::split(data)
            .iter()
            .filter_map(|n| annexb::nal_type(VideoCodec::Hevc, &data[n.payload..n.end]))
            .collect()
    }

    #[test]
    fn sei_bytes_match_hand_computed_encoding() {
        // Green, blue, red, white as big-endian u16 pairs, then the two
        // luminances. The min luminance 00 00 00 01 follows 80 00 ... so the
        // run "00 00 00" is escaped to "00 00 03 00".
        #[rustfmt::skip]
        let expected = [
            0x00, 0x00, 0x00, 0x01,             // start code
            0x4E, 0x01,                         // prefix SEI, layer 0, tid 0
            0x89, 0x18,                         // payloadType 137, size 24
            0x21, 0x34, 0x9B, 0xAA,             // green 8500, 39850
            0x19, 0x96, 0x08, 0xFC,             // blue 6550, 2300
            0x8A, 0x48, 0x39, 0x08,             // red 35400, 14600
            0x3D, 0x13, 0x40, 0x42,             // white 15635, 16450
            0x00, 0x98, 0x96, 0x80,             // max luminance 10_000_000
            0x00, 0x00, 0x03, 0x00, 0x01,       // min luminance 1, escaped
            0x90, 0x04,                         // payloadType 144, size 4
            0x03, 0xE8, 0x01, 0x90,             // max_cll 1000, max_fall 400
            0x80,                               // rbsp_trailing_bits
        ];
        assert_eq!(hevc_hdr10_sei(&meta()), expected);
    }

    #[test]
    fn zero_metadata_is_fully_escaped() {
        let zero = HdrMetadata {
            red: [0; 2],
            green: [0; 2],
            blue: [0; 2],
            white: [0; 2],
            max_luminance: 0,
            min_luminance: 0,
            max_cll: 0,
            max_fall: 0,
        };
        // 24 zero bytes: a 03 follows every pair except the last, which is
        // followed by the next payloadType (0x90 > 3). Same for the 4 zeros
        // of the light levels.
        let mut expected = vec![0, 0, 0, 1, 0x4E, 0x01, 0x89, 0x18];
        expected.extend([0, 0, 3].repeat(11));
        expected.extend([0, 0, 0x90, 0x04, 0, 0, 3, 0, 0, 0x80]);
        assert_eq!(hevc_hdr10_sei(&zero), expected);
    }

    #[test]
    fn escaping_rules() {
        let esc = |rbsp: &[u8]| {
            let mut out = Vec::new();
            escape_rbsp(&mut out, rbsp);
            out
        };
        assert_eq!(esc(&[0, 0, 0]), [0, 0, 3, 0, 3]); // trailing 00 gets a 03
        assert_eq!(esc(&[0, 0, 1]), [0, 0, 3, 1]);
        assert_eq!(esc(&[0, 0, 2]), [0, 0, 3, 2]);
        assert_eq!(esc(&[0, 0, 3]), [0, 0, 3, 3]); // a literal 03 is escaped too
        assert_eq!(esc(&[0, 0, 4]), [0, 0, 4]);
        assert_eq!(esc(&[0, 1, 0, 0, 5]), [0, 1, 0, 0, 5]);
        assert_eq!(esc(&[0, 0, 0, 0, 0, 7]), [0, 0, 3, 0, 0, 3, 0, 7]);
        assert_eq!(esc(&[1, 2, 0x80]), [1, 2, 0x80]);
    }

    #[test]
    fn escaped_sei_has_no_start_code_inside_and_round_trips() {
        for m in [meta(), HdrMetadata::bt2020_1000()] {
            let sei = hevc_hdr10_sei(&m);
            let body = &sei[4..];
            assert!(!body.windows(3).any(|w| w[0] == 0 && w[1] == 0 && w[2] <= 2));
            // Unescaping gives the unescaped RBSP: 2 + 24 + 2 + 4 + 1 bytes.
            let rbsp: Vec<u8> = Unescaped {
                data: &body[2..],
                zeros: 0,
            }
            .collect();
            assert_eq!(rbsp.len(), 33);
            assert_eq!(&rbsp[..2], [137, 24]);
            assert_eq!(&rbsp[26..28], [144, 4]);
            assert_eq!(rbsp[32], 0x80);
            assert_eq!(annexb::split(&sei).len(), 1);
        }
    }

    #[test]
    fn detects_mdcv_in_own_output_and_not_in_others() {
        let sei = hevc_hdr10_sei(&meta());
        assert!(hevc_has_mdcv_sei(&[AUD, &sei, IDR].concat()));
        assert!(!hevc_has_mdcv_sei(&[AUD, VPS, SPS, PPS, IDR].concat()));
        assert!(!hevc_has_mdcv_sei(&[SEI_OTHER, IDR].concat()));
        // A suffix SEI is not a prefix SEI, and an SEI after the slice is
        // out of scope.
        assert!(!hevc_has_mdcv_sei(&[IDR, SEI_SUFFIX_137].concat()));
        assert!(!hevc_has_mdcv_sei(&[IDR, &sei].concat()));
    }

    #[test]
    fn detects_mdcv_behind_other_messages_and_escapes() {
        // 144 (content light level) first, whose payload contains an
        // escaped 00 00 03 00 that must be skipped as 4 bytes, then 137.
        let nal = [
            0, 0, 0, 1, 0x4E, 0x01, // prefix SEI
            0x90, 0x04, 0x00, 0x00, 0x03, 0x00, 0x00, // CLL: 00 00 00 00 (escaped)
            0x89, 0x01, 0x00, // mdcv (truncated payload is fine here)
            0x80,
        ];
        assert!(hevc_has_mdcv_sei(&[&nal[..], IDR].concat()));
        // Extended payloadType: 0xFF 0x02 = 257 is not 137.
        let ext = [0, 0, 0, 1, 0x4E, 0x01, 0xFF, 0x02, 0x01, 0xAA, 0x80];
        assert!(!hevc_has_mdcv_sei(&[&ext[..], IDR].concat()));
        // Size larger than the NAL: no panic, no match.
        let short = [0, 0, 0, 1, 0x4E, 0x01, 0x05, 0x40, 0x01, 0x80];
        assert!(!hevc_has_mdcv_sei(&[&short[..], IDR].concat()));
        // Both messages in the same NAL after a user-data message.
        let sei = hevc_hdr10_sei(&meta());
        let mut two = vec![0, 0, 0, 1, 0x4E, 0x01, 0x05, 0x02, 0x12, 0x34];
        two.extend(&sei[6..]);
        assert!(hevc_has_mdcv_sei(&[&two[..], IDR].concat()));
    }

    #[test]
    fn inserts_before_first_slice_after_parameter_sets() {
        let sei = hevc_hdr10_sei(&meta());
        let au = [AUD, VPS, SPS, PPS, IDR].concat();
        let out = insert_hevc_hdr10_sei(&au, &sei);
        assert_eq!(out.as_ref(), [AUD, VPS, SPS, PPS, &sei, IDR].concat());
        assert_eq!(types(&out), [35, 32, 33, 34, 39, 19]);
        assert!(hevc_has_mdcv_sei(&out));
    }

    #[test]
    fn inserts_after_existing_prefix_sei() {
        let sei = hevc_hdr10_sei(&meta());
        let au = [AUD, VPS, SPS, PPS, SEI_OTHER, IDR].concat();
        let out = insert_hevc_hdr10_sei(&au, &sei);
        assert_eq!(
            out.as_ref(),
            [AUD, VPS, SPS, PPS, SEI_OTHER, &sei, IDR].concat()
        );
        assert_eq!(types(&out), [35, 32, 33, 34, 39, 39, 19]);
    }

    #[test]
    fn works_with_three_byte_slice_start_code_and_bare_slice() {
        let sei = hevc_hdr10_sei(&meta());
        let au = [SPS, IDR_3].concat();
        let out = insert_hevc_hdr10_sei(&au, &sei);
        assert_eq!(out.as_ref(), [SPS, &sei, IDR_3].concat());
        let bare = insert_hevc_hdr10_sei(IDR, &sei);
        assert_eq!(bare.as_ref(), [&sei[..], IDR].concat());
    }

    #[test]
    fn inserting_twice_does_not_duplicate() {
        let sei = hevc_hdr10_sei(&meta());
        let au = [AUD, VPS, SPS, PPS, IDR].concat();
        let once = insert_hevc_hdr10_sei(&au, &sei).into_owned();
        let twice = insert_hevc_hdr10_sei(&once, &sei);
        assert!(matches!(twice, Cow::Borrowed(_)));
        assert_eq!(twice.as_ref(), once);
        assert_eq!(types(&twice).iter().filter(|&&t| t == 39).count(), 1);
    }

    #[test]
    fn leaves_access_units_without_a_slice_alone() {
        let sei = hevc_hdr10_sei(&meta());
        let params = [AUD, VPS, SPS, PPS].concat();
        assert!(matches!(
            insert_hevc_hdr10_sei(&params, &sei),
            Cow::Borrowed(_)
        ));
        assert!(matches!(insert_hevc_hdr10_sei(&[], &sei), Cow::Borrowed(_)));
    }

    #[test]
    fn non_idr_slice_also_gets_the_position_right() {
        // The position logic only cares about the first VCL NAL.
        let sei = hevc_hdr10_sei(&meta());
        let out = insert_hevc_hdr10_sei(TRAIL, &sei);
        assert_eq!(out.as_ref(), [&sei[..], TRAIL].concat());
    }
}
