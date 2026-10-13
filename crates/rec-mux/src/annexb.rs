//! Annex-B byte stream handling: NAL splitting and conversion to the
//! length-prefixed form used inside MP4 samples.

use crate::error::MuxError;

/// Splits an Annex-B byte stream into NAL units (start codes removed, trailing
/// zero bytes trimmed, empty NALs skipped).
///
/// Bytes before the first start code must be zero (leading zero padding is
/// allowed); anything else is an error because it means the packet is not an
/// Annex-B stream.
pub(crate) fn split_nals(data: &[u8]) -> Result<Vec<&[u8]>, MuxError> {
    let mut nals = Vec::new();
    let Some((mut start, mut code_len)) = find_start_code(data, 0) else {
        return Err(MuxError::malformed("video packet", "no Annex-B start code"));
    };
    if data[..start].iter().any(|&b| b != 0) {
        return Err(MuxError::malformed(
            "video packet",
            "data before the first start code",
        ));
    }
    loop {
        let body = start + code_len;
        match find_start_code(data, body) {
            Some((next, next_len)) => {
                push_nal(&mut nals, &data[body..next]);
                start = next;
                code_len = next_len;
            }
            None => {
                push_nal(&mut nals, &data[body..]);
                break;
            }
        }
    }
    Ok(nals)
}

fn push_nal<'a>(nals: &mut Vec<&'a [u8]>, nal: &'a [u8]) {
    let end = nal.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    if end > 0 {
        nals.push(&nal[..end]);
    }
}

/// Finds the next `00 00 01` at or after `from`, returning the position where
/// the start code begins (including a preceding zero byte for the 4-byte form)
/// and its length.
fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= data.len() {
        if data[i + 2] > 1 {
            i += 3;
        } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if i > from && data[i - 1] == 0 {
                return Some((i - 1, 4));
            }
            return Some((i, 3));
        } else {
            i += 1;
        }
    }
    None
}

/// Appends `nal` to `out` with a 4-byte big-endian length prefix.
pub(crate) fn push_length_prefixed(out: &mut Vec<u8>, nal: &[u8]) -> Result<(), MuxError> {
    let len = u32::try_from(nal.len()).map_err(|_| MuxError::TooLarge("NAL unit"))?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(nal);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, // 4-byte code
            0, 0, 1, 0x68, 3, // 3-byte code
            0, 0, 0, 1, 0x65, 9, 9, 9,
        ];
        let nals = split_nals(&data).unwrap();
        assert_eq!(
            nals,
            [&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 9, 9, 9][..]]
        );
    }

    #[test]
    fn trims_trailing_zeros_and_skips_empty_nals() {
        let data = [
            0, 0, 1, 0x09, 0x10, 0, 0, 0, 0, 0, 1, 0, 0, 1, 0x41, 0xAA, 0, 0,
        ];
        let nals = split_nals(&data).unwrap();
        assert_eq!(nals, [&[0x09, 0x10][..], &[0x41, 0xAA][..]]);
    }

    #[test]
    fn keeps_emulation_prevented_zeros_inside_a_nal() {
        let data = [0, 0, 1, 0x65, 0, 0, 3, 0, 0, 3, 1, 7];
        let nals = split_nals(&data).unwrap();
        assert_eq!(nals, [&[0x65, 0, 0, 3, 0, 0, 3, 1, 7][..]]);
    }

    #[test]
    fn allows_leading_zero_padding() {
        let data = [0, 0, 0, 0, 0, 1, 0x65, 5];
        assert_eq!(split_nals(&data).unwrap(), [&[0x65, 5][..]]);
    }

    #[test]
    fn rejects_non_annexb_data() {
        assert!(split_nals(&[]).is_err());
        assert!(split_nals(&[0x65, 1, 2, 3]).is_err());
        // Length-prefixed data (4-byte length 5) is not Annex-B.
        assert!(split_nals(&[0, 0, 0, 5, 0x65, 1, 2, 3, 4]).is_err());
        assert!(split_nals(&[7, 0, 0, 1, 0x65]).is_err());
    }

    #[test]
    fn start_code_only_gives_no_nals() {
        assert!(split_nals(&[0, 0, 0, 1]).unwrap().is_empty());
    }

    #[test]
    fn length_prefix_is_big_endian_u32() {
        let mut out = Vec::new();
        push_length_prefixed(&mut out, &[1, 2, 3]).unwrap();
        assert_eq!(out, [0, 0, 0, 3, 1, 2, 3]);
    }
}
