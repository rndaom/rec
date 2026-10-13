//! Annex-B helpers: NAL unit splitting, parameter-set detection and caching.
//!
//! The packet contract (`rec_core::media`) requires every keyframe to carry
//! its parameter sets in-band. Hardware encoders usually do that already; when
//! one does not, [`ParameterSets::ensure_in_keyframe`] inserts the cached sets.

use std::borrow::Cow;

use rec_core::media::VideoCodec;

const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// One NAL unit inside an Annex-B buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nal {
    /// Offset of the first byte of the start code.
    pub start: usize,
    /// Offset of the NAL header (first byte after the start code).
    pub payload: usize,
    /// End offset (exclusive): the next start code or the end of the buffer.
    pub end: usize,
}

/// Lazily iterates over the NAL units of an Annex-B buffer. Bytes before the
/// first start code are ignored.
#[derive(Clone, Debug)]
pub struct NalIter<'a> {
    data: &'a [u8],
    /// Position to resume scanning for the next start code.
    pos: usize,
    /// Start-code and payload offsets of the NAL found but not yet returned.
    pending: Option<(usize, usize)>,
}

impl<'a> NalIter<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        let mut it = Self {
            data,
            pos: 0,
            pending: None,
        };
        it.pending = it.find_start_code();
        it
    }

    /// Finds the next start code at or after `pos`; returns (start, payload).
    fn find_start_code(&mut self) -> Option<(usize, usize)> {
        let d = self.data;
        let mut i = self.pos;
        while i + 3 <= d.len() {
            if d[i + 2] > 1 {
                // Fast skip: no start code can end at i+2.
                i += 3;
            } else if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
                // A preceding zero byte makes it a 4-byte start code.
                let start = if i > 0 && d[i - 1] == 0 { i - 1 } else { i };
                self.pos = i + 3;
                return Some((start, i + 3));
            } else {
                i += 1;
            }
        }
        self.pos = d.len();
        None
    }
}

impl Iterator for NalIter<'_> {
    type Item = Nal;

    fn next(&mut self) -> Option<Nal> {
        let (start, payload) = self.pending.take()?;
        self.pending = self.find_start_code();
        let end = self.pending.map_or(self.data.len(), |(next, _)| next);
        Some(Nal {
            start,
            payload,
            end,
        })
    }
}

/// Splits an Annex-B byte stream into NAL units.
pub fn split(data: &[u8]) -> Vec<Nal> {
    NalIter::new(data).collect()
}

/// NAL unit type of the header byte(s) starting at `nal`.
pub fn nal_type(codec: VideoCodec, nal: &[u8]) -> Option<u8> {
    let first = *nal.first()?;
    match codec {
        VideoCodec::H264 => Some(first & 0x1f),
        VideoCodec::Hevc => Some((first >> 1) & 0x3f),
        VideoCodec::Av1 => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ParamKind {
    Vps,
    Sps,
    Pps,
}

fn param_kind(codec: VideoCodec, t: u8) -> Option<ParamKind> {
    match (codec, t) {
        (VideoCodec::H264, 7) => Some(ParamKind::Sps),
        (VideoCodec::H264, 8) => Some(ParamKind::Pps),
        (VideoCodec::Hevc, 32) => Some(ParamKind::Vps),
        (VideoCodec::Hevc, 33) => Some(ParamKind::Sps),
        (VideoCodec::Hevc, 34) => Some(ParamKind::Pps),
        _ => None,
    }
}

fn is_aud(codec: VideoCodec, t: u8) -> bool {
    matches!((codec, t), (VideoCodec::H264, 9) | (VideoCodec::Hevc, 35))
}

/// True for an IDR slice NAL type (H.264 type 5, HEVC IDR_W_RADL/IDR_N_LP).
pub fn is_idr(codec: VideoCodec, t: u8) -> bool {
    matches!(
        (codec, t),
        (VideoCodec::H264, 5) | (VideoCodec::Hevc, 19) | (VideoCodec::Hevc, 20)
    )
}

/// True if `t` is a VCL (slice) NAL type.
pub fn is_vcl(codec: VideoCodec, t: u8) -> bool {
    match codec {
        VideoCodec::H264 => (1..=5).contains(&t),
        VideoCodec::Hevc => t <= 31,
        VideoCodec::Av1 => false,
    }
}

/// The NAL units before the first VCL (slice) NAL: AUD, parameter sets,
/// SEI. Stops at the first slice header, so it never scans picture data.
fn prefix_nals(codec: VideoCodec, data: &[u8]) -> Vec<Nal> {
    let mut it = NalIter {
        data,
        pos: 0,
        pending: None,
    };
    let mut out = Vec::new();
    let mut cur = it.find_start_code();
    while let Some((start, payload)) = cur {
        if nal_type(codec, &data[payload..]).is_some_and(|t| is_vcl(codec, t)) {
            break;
        }
        let next = it.find_start_code();
        out.push(Nal {
            start,
            payload,
            end: next.map_or(data.len(), |(n, _)| n),
        });
        cur = next;
    }
    out
}

/// Which parameter sets precede the first slice of `data`, as
/// `(vps, sps, pps)`.
fn present(codec: VideoCodec, data: &[u8]) -> (bool, bool, bool) {
    let mut found = (false, false, false);
    for nal in prefix_nals(codec, data) {
        match nal_type(codec, &data[nal.payload..nal.end]).and_then(|t| param_kind(codec, t)) {
            Some(ParamKind::Vps) => found.0 = true,
            Some(ParamKind::Sps) => found.1 = true,
            Some(ParamKind::Pps) => found.2 = true,
            None => {}
        }
    }
    found
}

fn complete(codec: VideoCodec, (vps, sps, pps): (bool, bool, bool)) -> bool {
    sps && pps && (vps || codec != VideoCodec::Hevc)
}

/// True if `data` carries a complete set of parameter sets for `codec`
/// (SPS + PPS, plus VPS for HEVC).
pub fn has_parameter_sets(codec: VideoCodec, data: &[u8]) -> bool {
    complete(codec, present(codec, data))
}

/// Type of the first VCL NAL in an access unit, if any. Only scans up to
/// that NAL's header (never through slice data), so it is cheap for any
/// frame size.
pub fn first_vcl_type(codec: VideoCodec, data: &[u8]) -> Option<u8> {
    let mut it = NalIter {
        data,
        pos: 0,
        pending: None,
    };
    while let Some((_, payload)) = it.find_start_code() {
        let t = nal_type(codec, data.get(payload..)?)?;
        if is_vcl(codec, t) {
            return Some(t);
        }
    }
    None
}

/// Cached parameter sets for one encoder session, stored as Annex-B with
/// 4-byte start codes in VPS, SPS, PPS order.
#[derive(Clone, Debug)]
pub struct ParameterSets {
    codec: VideoCodec,
    annexb: Vec<u8>,
}

/// What [`ParameterSets::ensure_in_keyframe`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyframeFix {
    /// The keyframe already carried its parameter sets (cache refreshed).
    AlreadyPresent,
    /// Cached parameter sets were inserted.
    Inserted,
    /// The keyframe has no parameter sets and none are cached yet.
    Missing,
}

impl ParameterSets {
    pub fn new(codec: VideoCodec) -> Self {
        Self {
            codec,
            annexb: Vec::new(),
        }
    }

    pub fn is_complete(&self) -> bool {
        has_parameter_sets(self.codec, &self.annexb)
    }

    pub fn as_annexb(&self) -> &[u8] {
        &self.annexb
    }

    /// Replaces the cache with the parameter sets found in `data` if `data`
    /// holds a complete set. Returns whether the cache was updated.
    pub fn update_from(&mut self, data: &[u8]) -> bool {
        if !has_parameter_sets(self.codec, data) {
            return false;
        }
        let nals = prefix_nals(self.codec, data);
        let mut out = Vec::new();
        for kind in [ParamKind::Vps, ParamKind::Sps, ParamKind::Pps] {
            for nal in &nals {
                let payload = &data[nal.payload..nal.end];
                if nal_type(self.codec, payload).and_then(|t| param_kind(self.codec, t))
                    == Some(kind)
                {
                    out.extend_from_slice(&START_CODE);
                    out.extend_from_slice(trim_trailing_zeros(payload));
                }
            }
        }
        self.annexb = out;
        true
    }

    /// Makes sure the keyframe access unit `data` begins with parameter sets.
    /// If it already has them the cache is refreshed from it; otherwise the
    /// cached sets are inserted at the start (after an access unit delimiter,
    /// which must stay first).
    pub fn ensure_in_keyframe<'a>(&mut self, data: &'a [u8]) -> (Cow<'a, [u8]>, KeyframeFix) {
        if self.update_from(data) {
            return (Cow::Borrowed(data), KeyframeFix::AlreadyPresent);
        }
        if !self.is_complete() {
            return (Cow::Borrowed(data), KeyframeFix::Missing);
        }
        let nals = prefix_nals(self.codec, data);
        let insert_at = match nals.first() {
            Some(first)
                if nal_type(self.codec, &data[first.payload..first.end])
                    .is_some_and(|t| is_aud(self.codec, t)) =>
            {
                first.end
            }
            _ => 0,
        };
        let mut out = Vec::with_capacity(data.len() + self.annexb.len());
        out.extend_from_slice(&data[..insert_at]);
        out.extend_from_slice(&self.annexb);
        out.extend_from_slice(&data[insert_at..]);
        (Cow::Owned(out), KeyframeFix::Inserted)
    }
}

/// `data` with `insert` spliced in at offset `at`; the buffer is copied once.
pub(crate) fn splice(data: &[u8], at: usize, insert: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + insert.len());
    out.extend_from_slice(&data[..at]);
    out.extend_from_slice(insert);
    out.extend_from_slice(&data[at..]);
    out
}

/// Drops trailing zero bytes (`trailing_zero_8bits`), which belong to the
/// stream rather than to the NAL unit.
fn trim_trailing_zeros(nal: &[u8]) -> &[u8] {
    let mut end = nal.len();
    while end > 1 && nal[end - 1] == 0 {
        end -= 1;
    }
    &nal[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    // HEVC NAL headers: type << 1, layer/tid byte 0x01.
    const H_VPS: &[u8] = &[0, 0, 0, 1, 0x40, 0x01, 0xAA];
    const H_SPS: &[u8] = &[0, 0, 0, 1, 0x42, 0x01, 0xBB];
    const H_PPS: &[u8] = &[0, 0, 1, 0x44, 0x01, 0xCC];
    const H_AUD: &[u8] = &[0, 0, 0, 1, 0x46, 0x01, 0x50];
    const H_IDR: &[u8] = &[0, 0, 0, 1, 0x26, 0x01, 0xDD, 0xEE];
    const H_TRAIL: &[u8] = &[0, 0, 0, 1, 0x02, 0x01, 0x11];

    // H.264: SPS 0x67, PPS 0x68, IDR 0x65, AUD 0x09, non-IDR 0x41.
    const A_SPS: &[u8] = &[0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28];
    const A_PPS: &[u8] = &[0, 0, 0, 1, 0x68, 0xEE, 0x3C, 0x80];
    const A_AUD: &[u8] = &[0, 0, 0, 1, 0x09, 0xF0];
    const A_IDR: &[u8] = &[0, 0, 1, 0x65, 0x88, 0x84];

    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    #[test]
    fn splits_3_and_4_byte_start_codes() {
        let data = cat(&[H_VPS, H_PPS, H_IDR]);
        let nals = split(&data);
        assert_eq!(nals.len(), 3);
        assert_eq!(
            nals[0],
            Nal {
                start: 0,
                payload: 4,
                end: 7
            }
        );
        assert_eq!(
            nals[1],
            Nal {
                start: 7,
                payload: 10,
                end: 13
            }
        );
        assert_eq!(nals[2].payload, 17);
        assert_eq!(nals[2].end, data.len());
        assert!(split(&[0x12, 0x34]).is_empty());
    }

    #[test]
    fn nal_types() {
        assert_eq!(nal_type(VideoCodec::Hevc, &H_IDR[4..]), Some(19));
        assert_eq!(nal_type(VideoCodec::Hevc, &H_VPS[4..]), Some(32));
        assert_eq!(nal_type(VideoCodec::H264, &A_IDR[3..]), Some(5));
        assert!(is_idr(VideoCodec::Hevc, 19) && is_idr(VideoCodec::Hevc, 20));
        assert!(!is_idr(VideoCodec::Hevc, 21)); // CRA is not an IDR
        assert_eq!(
            first_vcl_type(VideoCodec::Hevc, &cat(&[H_AUD, H_VPS, H_IDR])),
            Some(19)
        );
    }

    #[test]
    fn detects_complete_parameter_sets() {
        assert!(has_parameter_sets(
            VideoCodec::Hevc,
            &cat(&[H_VPS, H_SPS, H_PPS, H_IDR])
        ));
        // HEVC needs the VPS too.
        assert!(!has_parameter_sets(
            VideoCodec::Hevc,
            &cat(&[H_SPS, H_PPS, H_IDR])
        ));
        assert!(has_parameter_sets(
            VideoCodec::H264,
            &cat(&[A_SPS, A_PPS, A_IDR])
        ));
        assert!(!has_parameter_sets(VideoCodec::H264, &cat(&[A_SPS, A_IDR])));
    }

    #[test]
    fn keyframe_with_sets_is_untouched_and_cached() {
        let mut ps = ParameterSets::new(VideoCodec::Hevc);
        let key = cat(&[H_VPS, H_SPS, H_PPS, H_IDR]);
        let (out, fix) = ps.ensure_in_keyframe(&key);
        assert_eq!(fix, KeyframeFix::AlreadyPresent);
        assert!(matches!(out, Cow::Borrowed(_)));
        // The cache normalises to 4-byte start codes.
        assert_eq!(
            ps.as_annexb(),
            cat(&[H_VPS, H_SPS, &[0, 0, 0, 1, 0x44, 0x01, 0xCC]])
        );
    }

    #[test]
    fn missing_sets_are_prepended() {
        let mut ps = ParameterSets::new(VideoCodec::Hevc);
        let bare = cat(&[H_IDR]);
        assert_eq!(ps.ensure_in_keyframe(&bare).1, KeyframeFix::Missing);

        assert!(ps.update_from(&cat(&[H_VPS, H_SPS, H_PPS])));
        let (out, fix) = ps.ensure_in_keyframe(&bare);
        assert_eq!(fix, KeyframeFix::Inserted);
        assert!(has_parameter_sets(VideoCodec::Hevc, &out));
        assert_eq!(&out[..ps.as_annexb().len()], ps.as_annexb());
        assert!(out.ends_with(H_IDR));
    }

    #[test]
    fn sets_go_after_access_unit_delimiter() {
        let mut ps = ParameterSets::new(VideoCodec::H264);
        assert!(ps.update_from(&cat(&[A_SPS, A_PPS])));
        let key = cat(&[A_AUD, A_IDR]);
        let (out, fix) = ps.ensure_in_keyframe(&key);
        assert_eq!(fix, KeyframeFix::Inserted);
        assert_eq!(out.as_ref(), cat(&[A_AUD, A_SPS, A_PPS, A_IDR]));
        let types: Vec<u8> = split(&out)
            .iter()
            .filter_map(|n| nal_type(VideoCodec::H264, &out[n.payload..n.end]))
            .collect();
        assert_eq!(types, [9, 7, 8, 5]);
    }

    #[test]
    fn partial_sets_do_not_replace_cache() {
        let mut ps = ParameterSets::new(VideoCodec::Hevc);
        assert!(ps.update_from(&cat(&[H_VPS, H_SPS, H_PPS])));
        let before = ps.as_annexb().to_vec();
        assert!(!ps.update_from(&cat(&[H_SPS, H_TRAIL])));
        assert_eq!(ps.as_annexb(), before);
    }

    #[test]
    fn trailing_zero_bytes_are_trimmed() {
        let mut ps = ParameterSets::new(VideoCodec::H264);
        let data = cat(&[A_SPS, &[0, 0], A_PPS]);
        assert!(ps.update_from(&data));
        assert_eq!(ps.as_annexb(), cat(&[A_SPS, A_PPS]));
    }
}
