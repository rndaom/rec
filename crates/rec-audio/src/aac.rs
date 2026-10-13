//! AAC bitstream helpers: bitrate mapping, AudioSpecificConfig, ADTS.

/// Bitrates the Media Foundation AAC encoder accepts, in kbit/s. It only takes
/// these exact `MF_MT_AUDIO_AVG_BYTES_PER_SECOND` values (12000, 16000, 20000,
/// 24000 bytes/s).
pub const SUPPORTED_BITRATES_KBPS: [u32; 4] = [96, 128, 160, 192];

/// Maps a requested bitrate to the nearest supported one (ties go up).
pub fn nearest_bitrate_kbps(requested: u32) -> u32 {
    let mut best = SUPPORTED_BITRATES_KBPS[0];
    for &b in &SUPPORTED_BITRATES_KBPS {
        if b.abs_diff(requested) <= best.abs_diff(requested) {
            best = b;
        }
    }
    best
}

/// MPEG-4 audio object type for AAC-LC.
pub const OBJECT_TYPE_AAC_LC: u8 = 2;

const SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

fn rate_index(sample_rate: u32) -> Option<u8> {
    SAMPLE_RATES
        .iter()
        .position(|&r| r == sample_rate)
        .map(|i| i as u8)
}

/// The fields of an AudioSpecificConfig that matter to us.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AscInfo {
    pub object_type: u8,
    pub sample_rate: u32,
    pub channels: u8,
}

/// Builds the 2-byte AudioSpecificConfig for AAC-LC (ISO 14496-3 1.6.2.1).
/// `channels` is the channel configuration (1..=7).
pub fn build_asc(sample_rate: u32, channels: u8) -> Option<[u8; 2]> {
    let freq = rate_index(sample_rate)?;
    if !(1..=7).contains(&channels) {
        return None;
    }
    // objectType(5) | samplingFrequencyIndex(4) | channelConfiguration(4) |
    // frameLengthFlag(1) dependsOnCoreCoder(1) extensionFlag(1), all zero.
    let bits =
        (u16::from(OBJECT_TYPE_AAC_LC) << 11) | (u16::from(freq) << 7) | (u16::from(channels) << 3);
    Some(bits.to_be_bytes())
}

/// Parses the head of an AudioSpecificConfig.
pub fn parse_asc(asc: &[u8]) -> Option<AscInfo> {
    let mut r = BitReader { data: asc, pos: 0 };
    let mut object_type = r.read(5)? as u8;
    if object_type == 31 {
        object_type = 32 + r.read(6)? as u8;
    }
    let freq = r.read(4)?;
    let sample_rate = if freq == 15 {
        r.read(24)?
    } else {
        *SAMPLE_RATES.get(freq as usize)?
    };
    let channels = r.read(4)? as u8;
    Some(AscInfo {
        object_type,
        sample_rate,
        channels,
    })
}

/// Size of the `HEAACWAVEINFO` fields that follow `WAVEFORMATEX` (payload
/// type, profile/level, struct type and two reserved fields). In
/// `MF_MT_USER_DATA` of an AAC media type they precede the
/// AudioSpecificConfig.
pub const HEAACWAVEINFO_EXTRA_LEN: usize = 12;

/// Extracts the AudioSpecificConfig from `MF_MT_USER_DATA` of an AAC media
/// type and checks that it describes the expected stream.
pub fn asc_from_user_data(user_data: &[u8], expected: AscInfo) -> Option<Vec<u8>> {
    let asc = user_data.get(HEAACWAVEINFO_EXTRA_LEN..)?;
    (parse_asc(asc)? == expected).then(|| asc.to_vec())
}

/// Length of an ADTS header without CRC.
pub const ADTS_HEADER_LEN: usize = 7;

/// ADTS header (no CRC) for one raw AAC-LC access unit of `payload_len` bytes.
/// rec stores raw access units; ADTS is only for debug dumps and tests.
pub fn adts_header(sample_rate: u32, channels: u8, payload_len: usize) -> Option<[u8; 7]> {
    let freq = rate_index(sample_rate)?;
    let len = payload_len + ADTS_HEADER_LEN;
    if len >= 1 << 13 || channels > 7 {
        return None;
    }
    let profile = OBJECT_TYPE_AAC_LC - 1;
    Some([
        0xFF,
        0xF1, // sync, MPEG-4, layer 0, no CRC
        (profile << 6) | (freq << 2) | (channels >> 2),
        ((channels & 3) << 6) | (len >> 11) as u8,
        (len >> 3) as u8,
        (((len & 7) as u8) << 5) | 0x1F, // buffer fullness 0x7FF (VBR)
        0xFC,                            // ...and one raw data block
    ])
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl BitReader<'_> {
    fn read(&mut self, bits: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..bits {
            let byte = *self.data.get(self.pos / 8)?;
            v = (v << 1) | u32::from((byte >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LC_48K_STEREO: AscInfo = AscInfo {
        object_type: 2,
        sample_rate: 48_000,
        channels: 2,
    };

    #[test]
    fn bitrate_maps_to_nearest_supported() {
        assert_eq!(nearest_bitrate_kbps(0), 96);
        assert_eq!(nearest_bitrate_kbps(96), 96);
        assert_eq!(nearest_bitrate_kbps(111), 96);
        assert_eq!(nearest_bitrate_kbps(112), 128);
        assert_eq!(nearest_bitrate_kbps(160), 160);
        assert_eq!(nearest_bitrate_kbps(181), 192);
        assert_eq!(nearest_bitrate_kbps(320), 192);
    }

    #[test]
    fn asc_for_lc_48k_stereo() {
        assert_eq!(build_asc(48_000, 2), Some([0x11, 0x90]));
        assert_eq!(build_asc(44_100, 2), Some([0x12, 0x10]));
        assert_eq!(build_asc(48_000, 0), None);
        assert_eq!(build_asc(47_999, 2), None);
        assert_eq!(parse_asc(&[0x11, 0x90]), Some(LC_48K_STEREO));
        assert_eq!(parse_asc(&[0x11]), None);
    }

    #[test]
    fn asc_is_taken_from_heaacwaveinfo_user_data() {
        // What the MF AAC encoder reports: payload type 0 (raw), profile 0x29,
        // struct type 0, reserved, then the ASC.
        let user_data = [0, 0, 0x29, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x11, 0x90];
        assert_eq!(
            asc_from_user_data(&user_data, LC_48K_STEREO),
            Some(vec![0x11, 0x90])
        );
        // Wrong rate or truncated data is rejected.
        let wrong = [0, 0, 0x29, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x12, 0x10];
        assert_eq!(asc_from_user_data(&wrong, LC_48K_STEREO), None);
        assert_eq!(asc_from_user_data(&user_data[..12], LC_48K_STEREO), None);
    }

    #[test]
    fn adts_header_fields() {
        let h = adts_header(48_000, 2, 100).unwrap();
        assert_eq!(h[0], 0xFF);
        assert_eq!(h[1], 0xF1);
        // profile LC (1), freq index 3, channels 2.
        assert_eq!(h[2] >> 6, 1);
        assert_eq!((h[2] >> 2) & 0xF, 3);
        let channels = ((h[2] & 1) << 2) | (h[3] >> 6);
        assert_eq!(channels, 2);
        let len = (usize::from(h[3] & 3) << 11) | (usize::from(h[4]) << 3) | usize::from(h[5] >> 5);
        assert_eq!(len, 107);
        assert_eq!(adts_header(48_000, 2, 9000), None);
    }
}
