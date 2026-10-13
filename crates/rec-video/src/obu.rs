//! AV1 OBU helpers: splitting temporal units, key-frame detection, keeping the
//! sequence header in key frames and adding HDR10 metadata OBUs.
//!
//! The packet contract (`rec_core::media`) says an AV1 packet is one temporal
//! unit in the low-overhead format, every OBU with a size field, and every
//! key-frame packet carries the sequence header (the muxer builds `av1C`
//! from it) and, for HDR10, the `HDR_CLL` / `HDR_MDCV` metadata OBUs. Hardware
//! encoders emit the sequence header with the first key frame only and never
//! the metadata, so these helpers do for AV1 what [`crate::annexb`] and
//! [`crate::sei`] do for the Annex-B codecs. [`KeyframeFix`] is reused: the
//! sequence header plays the role of the parameter sets.
//!
//! Nothing here depends on rec-mux; the little bit of bit-level parsing is
//! limited to what the key-frame decision needs.
//!
//! Unlike HEVC, AV1 has no emulation prevention: payload bytes go out as is.

use std::borrow::Cow;

use rec_core::media::HdrMetadata;

use crate::annexb::{self, KeyframeFix};

pub const OBU_SEQUENCE_HEADER: u8 = 1;
pub const OBU_TEMPORAL_DELIMITER: u8 = 2;
pub const OBU_FRAME_HEADER: u8 = 3;
pub const OBU_METADATA: u8 = 5;
pub const OBU_FRAME: u8 = 6;

/// `metadata_type` of `metadata_hdr_cll` (content light level).
const METADATA_HDR_CLL: u8 = 1;
/// `metadata_type` of `metadata_hdr_mdcv` (mastering display colour volume).
const METADATA_HDR_MDCV: u8 = 2;
/// `obu_has_size_field`.
const HAS_SIZE_FIELD: u8 = 0x02;
/// `trailing_bits` of a byte-aligned payload: the stop bit and zero padding.
const TRAILING_BITS: u8 = 0x80;

/// A malformed temporal unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ObuError {
    #[error("OBU at byte {0} has the forbidden bit set")]
    ForbiddenBit(usize),
    #[error("OBU at byte {0} has no size field (the packet contract requires one)")]
    MissingSizeField(usize),
    #[error("OBU at byte {0} is truncated or has an invalid size")]
    Truncated(usize),
}

/// One OBU of a temporal unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Obu<'a> {
    /// `obu_type`.
    pub kind: u8,
    /// `obu_extension_header` (temporal/spatial id), if present.
    pub extension: Option<u8>,
    /// The OBU payload, without header and size field.
    pub payload: &'a [u8],
    /// The whole OBU as it appears in the stream: header, size field, payload.
    pub raw: &'a [u8],
    /// Offset of the OBU's first byte in the temporal unit.
    pub offset: usize,
}

impl Obu<'_> {
    fn end(&self) -> usize {
        self.offset + self.raw.len()
    }
}

/// Iterates over the OBUs of a temporal unit without allocating. After an
/// error it yields nothing more.
#[derive(Clone, Debug)]
pub struct ObuIter<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ObuIter<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn parse(&self) -> Result<Obu<'a>, ObuError> {
        let at = self.pos;
        let bad = ObuError::Truncated(at);
        let first = self.data[at];
        if first & 0x80 != 0 {
            return Err(ObuError::ForbiddenBit(at));
        }
        if first & HAS_SIZE_FIELD == 0 {
            return Err(ObuError::MissingSizeField(at));
        }
        let extension = if first & 0x04 != 0 {
            Some(*self.data.get(at + 1).ok_or(bad)?)
        } else {
            None
        };
        let size_at = at + 1 + usize::from(extension.is_some());
        let (len, used) = read_leb128(self.data.get(size_at..).ok_or(bad)?).ok_or(bad)?;
        let start = size_at + used;
        let end = usize::try_from(len)
            .ok()
            .and_then(|len| start.checked_add(len))
            .ok_or(bad)?;
        Ok(Obu {
            kind: (first >> 3) & 0x0F,
            extension,
            payload: self.data.get(start..end).ok_or(bad)?,
            raw: &self.data[at..end],
            offset: at,
        })
    }
}

impl<'a> Iterator for ObuIter<'a> {
    type Item = Result<Obu<'a>, ObuError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.data.len() {
            return None;
        }
        let obu = self.parse();
        self.pos = obu.as_ref().map_or(self.data.len(), Obu::end);
        Some(obu)
    }
}

/// Splits a temporal unit into its OBUs. An OBU without a size field, with
/// the forbidden bit set or running past the end of `tu` is an error.
pub fn split_obus(tu: &[u8]) -> Result<Vec<Obu<'_>>, ObuError> {
    ObuIter::new(tu).collect()
}

/// A `leb128()` value (at most 8 bytes) and the number of bytes it took.
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

/// Appends an OBU without extension, with a size field.
fn put_obu(out: &mut Vec<u8>, kind: u8, payload: &[u8]) {
    out.push((kind << 3) | HAS_SIZE_FIELD);
    put_leb128(out, payload.len());
    out.extend_from_slice(payload);
}

fn is_frame(kind: u8) -> bool {
    kind == OBU_FRAME || kind == OBU_FRAME_HEADER
}

/// `reduced_still_picture_header` from a sequence header payload:
/// `seq_profile` (3 bits), `still_picture` (1), `reduced_still_picture_header`.
fn reduced_still_picture_header(sequence_header_payload: &[u8]) -> bool {
    sequence_header_payload
        .first()
        .is_some_and(|b| b & 0x08 != 0)
}

/// Whether a frame OBU's uncompressed header starts a shown key frame.
fn starts_shown_key_frame(frame_payload: &[u8], reduced: bool) -> Option<bool> {
    if reduced {
        // show_existing_frame = 0, frame_type = KEY_FRAME, show_frame = 1.
        return Some(true);
    }
    let b = *frame_payload.first()?;
    let show_existing_frame = b & 0x80 != 0;
    let frame_type = (b >> 5) & 0x03;
    let show_frame = b & 0x10 != 0;
    // A shown existing frame is an old frame, not a new key frame for our
    // purposes; a hidden key frame would break the "no hidden frames" part
    // of the contract and is no random-access point for a clip.
    Some(!show_existing_frame && frame_type == 0 && show_frame)
}

/// Whether the shown frame of temporal unit `tu` is a new `KEY_FRAME`.
///
/// Parses just the first bits of the uncompressed header of the first
/// `OBU_FRAME` / `OBU_FRAME_HEADER`: `show_existing_frame` (then `false`),
/// `frame_type` and `show_frame`, so a hidden key frame is `false` too. With
/// `reduced_still_picture_header` the header has none of those bits and the
/// frame is always a key frame.
///
/// `reduced_still_picture_header` is the flag of the sequence header in
/// effect; a sequence header inside `tu` takes precedence over it (see
/// [`SequenceHeaderCache::is_key_frame`]). `None` if `tu` has no frame OBU
/// or cannot be parsed.
pub fn is_key_frame(tu: &[u8], reduced_still_picture_header: bool) -> Option<bool> {
    let mut reduced = reduced_still_picture_header;
    for obu in ObuIter::new(tu) {
        let obu = obu.ok()?;
        match obu.kind {
            OBU_SEQUENCE_HEADER => reduced = self::reduced_still_picture_header(obu.payload),
            k if is_frame(k) => return starts_shown_key_frame(obu.payload, reduced),
            _ => {}
        }
    }
    None
}

/// The parts of a temporal unit the insertion helpers need.
struct Scan<'a> {
    /// The last sequence header before the first frame OBU.
    sequence_header: Option<Obu<'a>>,
    /// Offset right after the temporal delimiter, or 0 without one.
    after_delimiter: usize,
    /// Offset of the first `OBU_FRAME` / `OBU_FRAME_HEADER`.
    first_frame: Option<usize>,
    /// Whether any metadata OBU is `HDR_MDCV`.
    has_mdcv: bool,
}

fn scan(tu: &[u8]) -> Result<Scan<'_>, ObuError> {
    let mut s = Scan {
        sequence_header: None,
        after_delimiter: 0,
        first_frame: None,
        has_mdcv: false,
    };
    for obu in ObuIter::new(tu) {
        let obu = obu?;
        match obu.kind {
            OBU_TEMPORAL_DELIMITER if obu.offset == 0 => s.after_delimiter = obu.end(),
            OBU_SEQUENCE_HEADER if s.first_frame.is_none() => s.sequence_header = Some(obu),
            OBU_METADATA => {
                s.has_mdcv |= read_leb128(obu.payload)
                    .is_some_and(|(kind, _)| kind == u64::from(METADATA_HDR_MDCV));
            }
            k if is_frame(k) => {
                s.first_frame.get_or_insert(obu.offset);
            }
            _ => {}
        }
    }
    Ok(s)
}

/// The latest AV1 sequence header OBU of one encoder session, so key frames
/// that come without one (encoders usually emit it only once) can be given
/// theirs, like [`annexb::ParameterSets`] does for H.264 / HEVC.
#[derive(Clone, Debug, Default)]
pub struct SequenceHeaderCache {
    /// The cached OBU as it appeared in the stream (with size field); empty
    /// until one was seen.
    obu: Vec<u8>,
    reduced_still_picture_header: bool,
}

impl SequenceHeaderCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The cached sequence header OBU, empty if none was seen yet.
    pub fn as_obu(&self) -> &[u8] {
        &self.obu
    }

    pub fn is_empty(&self) -> bool {
        self.obu.is_empty()
    }

    /// `reduced_still_picture_header` of the cached sequence header.
    pub fn reduced_still_picture_header(&self) -> bool {
        self.reduced_still_picture_header
    }

    /// Remembers the sequence header of `tu`, if it has one (the last, should
    /// there be several). Returns whether `tu` had one.
    pub fn update_from(&mut self, tu: &[u8]) -> Result<bool, ObuError> {
        Ok(scan(tu)?
            .sequence_header
            .map(|sh| self.store(&sh))
            .is_some())
    }

    fn store(&mut self, sh: &Obu<'_>) {
        if self.obu != sh.raw {
            self.obu.clear();
            self.obu.extend_from_slice(sh.raw);
        }
        self.reduced_still_picture_header = reduced_still_picture_header(sh.payload);
    }

    /// [`is_key_frame`] with the cached sequence header's
    /// `reduced_still_picture_header`.
    pub fn is_key_frame(&self, tu: &[u8]) -> Option<bool> {
        is_key_frame(tu, self.reduced_still_picture_header)
    }

    /// Makes sure the key-frame temporal unit `tu` carries a sequence header.
    /// If it has one the cache is refreshed from it; otherwise the cached one
    /// is inserted right after the temporal delimiter (at the start if there
    /// is none), which is where the spec puts it.
    pub fn ensure_in_key_frame<'a>(
        &mut self,
        tu: &'a [u8],
    ) -> Result<(Cow<'a, [u8]>, KeyframeFix), ObuError> {
        let s = scan(tu)?;
        if let Some(sh) = s.sequence_header {
            self.store(&sh);
            return Ok((Cow::Borrowed(tu), KeyframeFix::AlreadyPresent));
        }
        if self.obu.is_empty() {
            return Ok((Cow::Borrowed(tu), KeyframeFix::Missing));
        }
        let out = annexb::splice(tu, s.after_delimiter, &self.obu);
        Ok((Cow::Owned(out), KeyframeFix::Inserted))
    }
}

/// The two metadata OBUs that carry `meta` in an AV1 stream: `HDR_CLL`
/// followed by `HDR_MDCV`, each an `OBU_METADATA` with a size field and
/// trailing bits. Build once per session and put into key frames with
/// [`insert_hdr10_metadata`].
///
/// Unit conversions from [`HdrMetadata`] (rounded to nearest, clamped to the
/// field's range):
///
/// * chromaticities: 0.00002 units to 0.16 fixed point, `v * 65536 / 50000`;
/// * `luminance_max`: 0.0001 cd/m² to 24.8 fixed point, `v * 256 / 10000`;
/// * `luminance_min`: 0.0001 cd/m² to 18.14 fixed point, `v * 16384 / 10000`;
/// * `max_cll` / `max_fall` are cd/m² in both.
///
/// The primaries go out red, green, blue (AV1 `i = 0, 1, 2`), unlike the HEVC
/// SEI's green, blue, red.
pub fn hdr10_metadata_obus(meta: &HdrMetadata) -> Vec<u8> {
    let mut cll = Vec::with_capacity(6);
    cll.push(METADATA_HDR_CLL);
    cll.extend(meta.max_cll.to_be_bytes());
    cll.extend(meta.max_fall.to_be_bytes());
    cll.push(TRAILING_BITS);

    let mut mdcv = Vec::with_capacity(26);
    mdcv.push(METADATA_HDR_MDCV);
    for [x, y] in [meta.red, meta.green, meta.blue, meta.white] {
        mdcv.extend(chromaticity_0_16(x).to_be_bytes());
        mdcv.extend(chromaticity_0_16(y).to_be_bytes());
    }
    mdcv.extend(luminance_fixed(meta.max_luminance, 256).to_be_bytes());
    mdcv.extend(luminance_fixed(meta.min_luminance, 16384).to_be_bytes());
    mdcv.push(TRAILING_BITS);

    let mut out = Vec::with_capacity(cll.len() + mdcv.len() + 4);
    put_obu(&mut out, OBU_METADATA, &cll);
    put_obu(&mut out, OBU_METADATA, &mdcv);
    out
}

/// 0.00002 units to 0.16 fixed point (1.0 is not representable: clamps).
fn chromaticity_0_16(v: u16) -> u16 {
    ((u64::from(v) * 65536 + 25_000) / 50_000).min(u64::from(u16::MAX)) as u16
}

/// 0.0001 cd/m² to fixed point with `scale` = 2^fraction_bits.
fn luminance_fixed(v: u32, scale: u64) -> u32 {
    ((u64::from(v) * scale + 5_000) / 10_000).min(u64::from(u32::MAX)) as u32
}

/// True if any metadata OBU of `tu` is `HDR_MDCV`.
pub fn has_hdr_mdcv_metadata(tu: &[u8]) -> Result<bool, ObuError> {
    Ok(scan(tu)?.has_mdcv)
}

/// Inserts `metadata_obus` (from [`hdr10_metadata_obus`]) into the key-frame
/// temporal unit `tu` right before its first frame OBU, which puts it after
/// the sequence header. The temporal unit is copied once.
///
/// `tu` is returned borrowed and unchanged if it already has `HDR_MDCV`
/// metadata (so applying this twice never duplicates it) or has no frame OBU
/// to put the metadata in front of.
pub fn insert_hdr10_metadata<'a>(
    tu: &'a [u8],
    metadata_obus: &[u8],
) -> Result<Cow<'a, [u8]>, ObuError> {
    let s = scan(tu)?;
    Ok(match s.first_frame {
        Some(at) if !s.has_mdcv => Cow::Owned(annexb::splice(tu, at, metadata_obus)),
        _ => Cow::Borrowed(tu),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obu(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        put_obu(&mut out, kind, payload);
        out
    }

    fn kinds(tu: &[u8]) -> Vec<u8> {
        split_obus(tu).unwrap().iter().map(|o| o.kind).collect()
    }

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

    fn td() -> Vec<u8> {
        obu(OBU_TEMPORAL_DELIMITER, &[])
    }

    /// Sequence header payloads: only the first byte (profile, still_picture,
    /// reduced_still_picture_header) matters here.
    fn seq_full() -> Vec<u8> {
        obu(OBU_SEQUENCE_HEADER, &[0x00, 0x11, 0x22])
    }

    fn seq_reduced() -> Vec<u8> {
        obu(OBU_SEQUENCE_HEADER, &[0x18, 0x33])
    }

    /// OBU_FRAME whose first header byte is `b`.
    fn frame(b: u8) -> Vec<u8> {
        obu(OBU_FRAME, &[b, 0xAB, 0xCD])
    }

    // First byte of the uncompressed header: show_existing_frame (bit 7),
    // frame_type (bits 6-5: key, inter, intra-only, switch), show_frame (bit 4).
    const KEY_SHOWN: u8 = 0x10;
    const KEY_HIDDEN: u8 = 0x00;
    const INTER_SHOWN: u8 = 0x30;
    const INTRA_ONLY_SHOWN: u8 = 0x50;
    const SWITCH_SHOWN: u8 = 0x70;
    const SHOW_EXISTING: u8 = 0x80;

    #[test]
    fn splits_obus_with_offsets() {
        let tu = [td(), seq_full(), frame(KEY_SHOWN)].concat();
        let obus = split_obus(&tu).unwrap();
        assert_eq!(kinds(&tu), [2, 1, 6]);
        assert_eq!(obus[0].raw, [0x12, 0x00]);
        assert!(obus[0].payload.is_empty());
        assert_eq!(obus[1].offset, 2);
        assert_eq!(obus[1].payload, [0x00, 0x11, 0x22]);
        assert_eq!(obus[1].raw, [0x0A, 0x03, 0x00, 0x11, 0x22]);
        assert_eq!(obus[2].offset, 7);
        assert_eq!(obus[2].payload, [KEY_SHOWN, 0xAB, 0xCD]);
        assert_eq!(obus[2].extension, None);
        assert!(split_obus(&[]).unwrap().is_empty());
    }

    #[test]
    fn handles_extension_byte_and_multibyte_sizes() {
        // OBU_FRAME with extension: header 0x36 (type 6, ext, size), ext 0x28.
        let mut tu = vec![0x36, 0x28, 0x02, KEY_SHOWN, 0x77];
        // 200-byte payload: size is the two-byte leb128 C8 01.
        let big = vec![0x5A; 200];
        tu.extend(obu(OBU_METADATA, &big));
        let obus = split_obus(&tu).unwrap();
        assert_eq!(obus[0].extension, Some(0x28));
        assert_eq!(obus[0].payload, [KEY_SHOWN, 0x77]);
        assert_eq!(obus[0].raw.len(), 5);
        assert_eq!(obus[1].offset, 5);
        assert_eq!(&obus[1].raw[..3], [0x2A, 0xC8, 0x01]);
        assert_eq!(obus[1].payload, big);
    }

    #[test]
    fn rejects_malformed_units() {
        // An OBU_FRAME header byte without obu_has_size_field, after a TD.
        let tu = [td(), vec![0x30, 0x01, 0x02]].concat();
        assert_eq!(split_obus(&tu), Err(ObuError::MissingSizeField(2)));
        // Forbidden bit.
        assert_eq!(split_obus(&[0x92, 0x00]), Err(ObuError::ForbiddenBit(0)));
        // Payload runs past the end.
        assert_eq!(split_obus(&[0x32, 0x05, 1, 2]), Err(ObuError::Truncated(0)));
        // Size field cut off, extension cut off, unterminated leb128.
        assert_eq!(split_obus(&[0x32]), Err(ObuError::Truncated(0)));
        assert_eq!(split_obus(&[0x36]), Err(ObuError::Truncated(0)));
        assert_eq!(split_obus(&[0x32, 0x80]), Err(ObuError::Truncated(0)));
        let endless = [vec![0x32], vec![0x80; 9]].concat();
        assert_eq!(split_obus(&endless), Err(ObuError::Truncated(0)));
        // The iterator stops after an error.
        let mut it = ObuIter::new(&[0x32, 0x09]);
        assert!(it.next().unwrap().is_err());
        assert!(it.next().is_none());
    }

    #[test]
    fn key_frame_detection() {
        let key = [td(), seq_full(), frame(KEY_SHOWN)].concat();
        assert_eq!(is_key_frame(&key, false), Some(true));
        assert_eq!(is_key_frame(&frame(KEY_SHOWN), false), Some(true));
        for (b, expected) in [
            (KEY_SHOWN, true),
            (KEY_HIDDEN, false),
            (INTER_SHOWN, false),
            (INTRA_ONLY_SHOWN, false),
            (SWITCH_SHOWN, false),
            (SHOW_EXISTING, false),
            // show_existing_frame wins over whatever follows it.
            (SHOW_EXISTING | KEY_SHOWN, false),
        ] {
            let tu = [td(), frame(b)].concat();
            assert_eq!(is_key_frame(&tu, false), Some(expected), "{b:#010b}");
        }
        // OBU_FRAME_HEADER counts like OBU_FRAME.
        let header = [td(), obu(OBU_FRAME_HEADER, &[KEY_SHOWN]), obu(4, &[1, 2])].concat();
        assert_eq!(is_key_frame(&header, false), Some(true));
        let inter = [td(), obu(OBU_FRAME_HEADER, &[INTER_SHOWN])].concat();
        assert_eq!(is_key_frame(&inter, false), Some(false));
    }

    #[test]
    fn key_frame_detection_edge_cases() {
        // No frame OBU, empty frame payload and garbage all give None.
        assert_eq!(is_key_frame(&td(), false), None);
        assert_eq!(is_key_frame(&[td(), seq_full()].concat(), false), None);
        assert_eq!(is_key_frame(&obu(OBU_FRAME, &[]), false), None);
        assert_eq!(is_key_frame(&[0x32, 0x09], false), None);
        assert_eq!(is_key_frame(&[], false), None);
        // The frame found is the first one, even if later OBUs are broken.
        let tu = [frame(KEY_SHOWN), vec![0x32, 0x09]].concat();
        assert_eq!(is_key_frame(&tu, false), Some(true));
    }

    #[test]
    fn reduced_still_picture_header_frames_are_key_frames() {
        // The header has no show_existing_frame / frame_type bits, so a first
        // byte that would read as "inter" or "show existing" is ignored.
        for b in [KEY_SHOWN, INTER_SHOWN, SHOW_EXISTING, 0xFF] {
            let tu = [td(), seq_reduced(), frame(b)].concat();
            assert_eq!(is_key_frame(&tu, false), Some(true));
            // ...and the flag can come from outside the unit instead.
            assert_eq!(is_key_frame(&frame(b), true), Some(true));
        }
        // A sequence header inside the unit beats the parameter, both ways.
        let full = [seq_full(), frame(INTER_SHOWN)].concat();
        assert_eq!(is_key_frame(&full, true), Some(false));
        // still_picture alone (bit 4) is not the reduced flag (bit 3).
        let still = [obu(OBU_SEQUENCE_HEADER, &[0x10]), frame(INTER_SHOWN)].concat();
        assert_eq!(is_key_frame(&still, true), Some(false));
        assert!(reduced_still_picture_header(&[0x18]));
        assert!(reduced_still_picture_header(&[0x08]));
        assert!(!reduced_still_picture_header(&[0x10]));
        assert!(!reduced_still_picture_header(&[]));
    }

    #[test]
    fn cache_inserts_after_temporal_delimiter() {
        let mut cache = SequenceHeaderCache::new();
        assert!(cache.is_empty());
        let first = [td(), seq_full(), frame(KEY_SHOWN)].concat();
        let (out, fix) = cache.ensure_in_key_frame(&first).unwrap();
        assert_eq!(fix, KeyframeFix::AlreadyPresent);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(cache.as_obu(), seq_full());

        let later = [td(), frame(KEY_SHOWN)].concat();
        let (out, fix) = cache.ensure_in_key_frame(&later).unwrap();
        assert_eq!(fix, KeyframeFix::Inserted);
        assert_eq!(out.as_ref(), [td(), seq_full(), frame(KEY_SHOWN)].concat());
        assert_eq!(kinds(&out), [2, 1, 6]);
    }

    #[test]
    fn cache_inserts_at_start_without_delimiter() {
        let mut cache = SequenceHeaderCache::new();
        assert!(
            cache
                .update_from(&[seq_full(), frame(KEY_SHOWN)].concat())
                .unwrap()
        );
        let bare = frame(KEY_SHOWN);
        let (out, fix) = cache.ensure_in_key_frame(&bare).unwrap();
        assert_eq!(fix, KeyframeFix::Inserted);
        assert_eq!(out.as_ref(), [seq_full(), frame(KEY_SHOWN)].concat());
    }

    #[test]
    fn cache_without_header_reports_missing() {
        let mut cache = SequenceHeaderCache::new();
        let tu = [td(), frame(KEY_SHOWN)].concat();
        let (out, fix) = cache.ensure_in_key_frame(&tu).unwrap();
        assert_eq!(fix, KeyframeFix::Missing);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert!(cache.is_empty());
    }

    #[test]
    fn cache_keeps_the_latest_header_and_is_idempotent() {
        let mut cache = SequenceHeaderCache::new();
        assert!(
            cache
                .update_from(&[td(), seq_full(), frame(KEY_SHOWN)].concat())
                .unwrap()
        );
        assert!(!cache.reduced_still_picture_header());
        let reduced = [td(), seq_reduced(), frame(KEY_SHOWN)].concat();
        let (_, fix) = cache.ensure_in_key_frame(&reduced).unwrap();
        assert_eq!(fix, KeyframeFix::AlreadyPresent);
        assert_eq!(cache.as_obu(), seq_reduced());
        assert!(cache.reduced_still_picture_header());
        // The cached flag feeds is_key_frame for header-less units.
        assert_eq!(cache.is_key_frame(&frame(INTER_SHOWN)), Some(true));

        // Fixing a fixed unit changes nothing and does not duplicate.
        let bare = [td(), frame(KEY_SHOWN)].concat();
        let fixed = cache.ensure_in_key_frame(&bare).unwrap().0.into_owned();
        let (again, fix) = cache.ensure_in_key_frame(&fixed).unwrap();
        assert_eq!(fix, KeyframeFix::AlreadyPresent);
        assert_eq!(again.as_ref(), fixed);
        assert_eq!(kinds(&again), [2, 1, 6]);
        // A TU without a sequence header leaves the cache alone.
        assert!(!cache.update_from(&bare).unwrap());
        assert_eq!(cache.as_obu(), seq_reduced());
    }

    #[test]
    fn cache_ignores_sequence_headers_after_the_frame_and_rejects_bad_units() {
        let mut cache = SequenceHeaderCache::new();
        let odd = [td(), frame(KEY_SHOWN), seq_full()].concat();
        assert!(!cache.update_from(&odd).unwrap());
        assert!(cache.ensure_in_key_frame(&[0x32]).is_err());
        assert_eq!(cache.is_key_frame(&[0x32]), None);
        // A header with an extension byte is cached verbatim.
        let ext = [0x0E, 0x08, 0x01, 0x00];
        let tu = [td(), ext.to_vec(), frame(KEY_SHOWN)].concat();
        assert!(cache.update_from(&tu).unwrap());
        assert_eq!(cache.as_obu(), ext);
    }

    #[test]
    fn metadata_obus_are_byte_exact() {
        #[rustfmt::skip]
        let expected = [
            // HDR_CLL: OBU_METADATA with size field, 6-byte payload.
            0x2A, 0x06,
            0x01,                               // metadata_type HDR_CLL
            0x03, 0xE8, 0x01, 0x90,             // max_cll 1000, max_fall 400
            0x80,                               // trailing_bits
            // HDR_MDCV: 26-byte payload.
            0x2A, 0x1A,
            0x02,                               // metadata_type HDR_MDCV
            0xB5, 0x3F, 0x4A, 0xC1,             // red 35400, 14600 -> 0.16
            0x2B, 0x85, 0xCC, 0x08,             // green 8500, 39850
            0x21, 0x89, 0x0B, 0xC7,             // blue 6550, 2300
            0x50, 0x0D, 0x54, 0x39,             // white 15635, 16450
            0x00, 0x03, 0xE8, 0x00,             // 1000 cd/m2 in 24.8 = 256000
            0x00, 0x00, 0x00, 0x02,             // 0.0001 cd/m2 in 18.14 = 1.64 -> 2
            0x80,                               // trailing_bits
        ];
        assert_eq!(hdr10_metadata_obus(&meta()), expected);
        assert_eq!(kinds(&hdr10_metadata_obus(&meta())), [5, 5]);
    }

    #[test]
    fn metadata_zero_bytes_are_not_escaped() {
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
        // AV1 has no emulation prevention: runs of 00 go out as they are.
        let mut expected = vec![0x2A, 0x06, 0x01, 0, 0, 0, 0, 0x80, 0x2A, 0x1A, 0x02];
        expected.extend([0; 24]);
        expected.push(0x80);
        assert_eq!(hdr10_metadata_obus(&zero), expected);
    }

    #[test]
    fn metadata_conversions_round_and_clamp() {
        // 1.0 (50000) does not fit 0.16 fixed point; half is exactly 0x8000.
        assert_eq!(chromaticity_0_16(50_000), 0xFFFF);
        assert_eq!(chromaticity_0_16(25_000), 0x8000);
        assert_eq!(chromaticity_0_16(u16::MAX), 0xFFFF);
        assert_eq!(chromaticity_0_16(0), 0);
        // 8500 * 65536 / 50000 = 11141.12; 6550 -> 8585.216 -> 8585 (down),
        // 2300 -> 3014.656 -> 3015 (up).
        assert_eq!(chromaticity_0_16(8_500), 11_141);
        assert_eq!(chromaticity_0_16(6_550), 8_585);
        assert_eq!(chromaticity_0_16(2_300), 3_015);
        // 24.8: 603.7 cd/m2 = 6_037_000 -> 154547.2 -> 154547.
        assert_eq!(luminance_fixed(6_037_000, 256), 154_547);
        // 18.14: 0.0491 cd/m2 = 491 -> 804.4544 -> 804; 0.0001 -> 2.
        assert_eq!(luminance_fixed(491, 16_384), 804);
        assert_eq!(luminance_fixed(1, 16_384), 2);
        assert_eq!(luminance_fixed(0, 16_384), 0);
        // Largest inputs stay in range (the 18.14 value saturates u32).
        assert_eq!(luminance_fixed(u32::MAX, 256), 109_951_163);
        assert_eq!(luminance_fixed(u32::MAX, 16_384), u32::MAX);
    }

    #[test]
    fn metadata_keeps_max_values() {
        let max = HdrMetadata {
            red: [u16::MAX; 2],
            green: [u16::MAX; 2],
            blue: [u16::MAX; 2],
            white: [u16::MAX; 2],
            max_luminance: u32::MAX,
            min_luminance: u32::MAX,
            max_cll: u16::MAX,
            max_fall: u16::MAX,
        };
        let obus = split_obus(&hdr10_metadata_obus(&max)).unwrap().len();
        assert_eq!(obus, 2);
        let bytes = hdr10_metadata_obus(&max);
        assert_eq!(&bytes[3..7], [0xFF; 4]);
        // Last luminance_min (u32::MAX clamped) then the trailing bits.
        assert_eq!(&bytes[bytes.len() - 5..], [0xFF, 0xFF, 0xFF, 0xFF, 0x80]);
    }

    #[test]
    fn metadata_goes_after_sequence_header_before_frame() {
        let md = hdr10_metadata_obus(&meta());
        let tu = [td(), seq_full(), frame(KEY_SHOWN)].concat();
        let out = insert_hdr10_metadata(&tu, &md).unwrap();
        assert_eq!(
            out.as_ref(),
            [td(), seq_full(), md.clone(), frame(KEY_SHOWN)].concat()
        );
        assert_eq!(kinds(&out), [2, 1, 5, 5, 6]);
        assert!(has_hdr_mdcv_metadata(&out).unwrap());
        assert!(!has_hdr_mdcv_metadata(&tu).unwrap());
    }

    #[test]
    fn metadata_insertion_is_idempotent() {
        let md = hdr10_metadata_obus(&meta());
        let tu = [td(), seq_full(), frame(KEY_SHOWN)].concat();
        let once = insert_hdr10_metadata(&tu, &md).unwrap().into_owned();
        let twice = insert_hdr10_metadata(&once, &md).unwrap();
        assert!(matches!(twice, Cow::Borrowed(_)));
        assert_eq!(twice.as_ref(), once);
        assert_eq!(kinds(&twice).iter().filter(|&&k| k == 5).count(), 2);
    }

    #[test]
    fn existing_mdcv_blocks_insertion_but_other_metadata_does_not() {
        let md = hdr10_metadata_obus(&meta());
        // The encoder's own HDR_MDCV (even without a CLL) is left alone.
        let own = obu(OBU_METADATA, &[METADATA_HDR_MDCV, 0x80]);
        let tu = [td(), seq_full(), own, frame(KEY_SHOWN)].concat();
        assert!(matches!(
            insert_hdr10_metadata(&tu, &md).unwrap(),
            Cow::Borrowed(_)
        ));
        // HDR_CLL only or an ITU-T T.35 message (type 4) does not count.
        let cll = obu(OBU_METADATA, &[METADATA_HDR_CLL, 0, 1, 0, 1, 0x80]);
        let t35 = obu(OBU_METADATA, &[4, 0xB5, 0x80]);
        let tu = [td(), seq_full(), cll, t35, frame(KEY_SHOWN)].concat();
        assert!(!has_hdr_mdcv_metadata(&tu).unwrap());
        let out = insert_hdr10_metadata(&tu, &md).unwrap();
        assert_eq!(kinds(&out), [2, 1, 5, 5, 5, 5, 6]);
        assert!(has_hdr_mdcv_metadata(&out).unwrap());
    }

    #[test]
    fn metadata_without_frame_or_with_bad_units() {
        let md = hdr10_metadata_obus(&meta());
        let tu = [td(), seq_full()].concat();
        assert!(matches!(
            insert_hdr10_metadata(&tu, &md).unwrap(),
            Cow::Borrowed(_)
        ));
        assert!(insert_hdr10_metadata(&[0x32, 0x09], &md).is_err());
        assert!(has_hdr_mdcv_metadata(&[0x30, 0x00]).is_err());
        // No sequence header in the unit: still goes right before the frame.
        let tu = [td(), frame(KEY_SHOWN)].concat();
        let out = insert_hdr10_metadata(&tu, &md).unwrap();
        assert_eq!(kinds(&out), [2, 5, 5, 6]);
    }

    #[test]
    fn full_key_frame_path() {
        // What the pipeline will do: restore the sequence header, then add
        // the HDR metadata; the result is a valid, ordered unit.
        let md = hdr10_metadata_obus(&meta());
        let mut cache = SequenceHeaderCache::new();
        cache
            .update_from(&[td(), seq_full(), frame(KEY_SHOWN)].concat())
            .unwrap();
        let tu = [td(), frame(KEY_SHOWN)].concat();
        assert_eq!(cache.is_key_frame(&tu), Some(true));
        let with_seq = cache.ensure_in_key_frame(&tu).unwrap().0.into_owned();
        let out = insert_hdr10_metadata(&with_seq, &md).unwrap();
        assert_eq!(kinds(&out), [2, 1, 5, 5, 6]);
        assert_eq!(cache.is_key_frame(&out), Some(true));
    }
}
