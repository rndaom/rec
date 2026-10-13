//! `moof` construction.

use crate::boxes::{Put, boxed, full_box};

/// One sample of a `trun`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TrunSample {
    pub duration: u32,
    pub size: u32,
    /// Only written when the traf says [`TrafSpec::per_sample_flags`].
    pub flags: u32,
}

pub(crate) struct TrafSpec<'a> {
    pub track_id: u32,
    pub base_media_decode_time: u64,
    pub per_sample_flags: bool,
    pub samples: &'a [TrunSample],
}

impl TrafSpec<'_> {
    pub(crate) fn payload_len(&self) -> u64 {
        self.samples.iter().map(|s| u64::from(s.size)).sum()
    }
}

/// Builds a `moof` whose `trun`s point into the `mdat` that directly follows
/// it (`default-base-is-moof`). Payloads of the trafs are laid out in order.
/// Returns `None` if an offset does not fit the 32-bit fields.
pub(crate) fn build_moof(sequence: u32, trafs: &[TrafSpec<'_>]) -> Option<Vec<u8>> {
    let mut buf =
        Vec::with_capacity(256 + trafs.iter().map(|t| t.samples.len() * 12).sum::<usize>());
    let mut offset_slots = Vec::with_capacity(trafs.len());
    boxed(&mut buf, b"moof", |b| {
        full_box(b, b"mfhd", 0, 0, |b| b.put_u32(sequence));
        for t in trafs {
            boxed(b, b"traf", |b| {
                // 0x020000: default-base-is-moof
                full_box(b, b"tfhd", 0, 0x02_0000, |b| b.put_u32(t.track_id));
                full_box(b, b"tfdt", 1, 0, |b| b.put_u64(t.base_media_decode_time));
                // data-offset | sample-duration | sample-size [| sample-flags]
                let flags = 0x001 | 0x100 | 0x200 | if t.per_sample_flags { 0x400 } else { 0 };
                full_box(b, b"trun", 0, flags, |b| {
                    b.put_u32(t.samples.len() as u32);
                    offset_slots.push(b.len());
                    b.put_u32(0); // data_offset, patched below
                    for s in t.samples {
                        b.put_u32(s.duration);
                        b.put_u32(s.size);
                        if t.per_sample_flags {
                            b.put_u32(s.flags);
                        }
                    }
                });
            });
        }
    });
    // mdat header (8 bytes) follows the moof.
    let mut offset = buf.len() as u64 + 8;
    for (slot, t) in offset_slots.into_iter().zip(trafs) {
        buf[slot..slot + 4].copy_from_slice(&u32::try_from(offset).ok()?.to_be_bytes());
        offset += t.payload_len();
    }
    // The last offset is the end of the payload, which must also fit.
    u32::try_from(offset).ok()?;
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be32(b: &[u8], at: usize) -> u32 {
        u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn moof_layout_and_data_offsets() {
        let video = [
            TrunSample {
                duration: 3000,
                size: 100,
                flags: 0x0200_0000,
            },
            TrunSample {
                duration: 3000,
                size: 50,
                flags: 0x0101_0000,
            },
        ];
        let audio = [TrunSample {
            duration: 1024,
            size: 30,
            flags: 0,
        }];
        let trafs = [
            TrafSpec {
                track_id: 1,
                base_media_decode_time: 90_000,
                per_sample_flags: true,
                samples: &video,
            },
            TrafSpec {
                track_id: 2,
                base_media_decode_time: 48_000,
                per_sample_flags: false,
                samples: &audio,
            },
        ];
        let moof = build_moof(7, &trafs).unwrap();
        assert_eq!(be32(&moof, 0) as usize, moof.len());
        assert_eq!(&moof[4..8], b"moof");
        // mfhd
        assert_eq!(&moof[12..16], b"mfhd");
        assert_eq!(be32(&moof, 20), 7);
        // First traf: size, 'traf', tfhd (16 bytes) then tfdt (v1, 20 bytes) then trun.
        let traf = 24;
        assert_eq!(&moof[traf + 4..traf + 8], b"traf");
        assert_eq!(&moof[traf + 8 + 4..traf + 8 + 8], b"tfhd");
        assert_eq!(be32(&moof, traf + 8 + 8), 0x0002_0000); // version 0, flags
        assert_eq!(be32(&moof, traf + 8 + 12), 1);
        let tfdt = traf + 8 + 16;
        assert_eq!(&moof[tfdt + 4..tfdt + 8], b"tfdt");
        assert_eq!(moof[tfdt + 8], 1);
        assert_eq!(
            u64::from_be_bytes(moof[tfdt + 12..tfdt + 20].try_into().unwrap()),
            90_000
        );
        let trun = tfdt + 20;
        assert_eq!(&moof[trun + 4..trun + 8], b"trun");
        assert_eq!(be32(&moof, trun + 8), 0x0000_0701);
        assert_eq!(be32(&moof, trun + 12), 2);
        assert_eq!(be32(&moof, trun + 16) as usize, moof.len() + 8);
        assert_eq!(be32(&moof, trun + 20), 3000);
        assert_eq!(be32(&moof, trun + 24), 100);
        assert_eq!(be32(&moof, trun + 28), 0x0200_0000);
        // Second traf's data offset follows the first payload (150 bytes).
        let second_trun = moof.windows(4).rposition(|w| w == b"trun").unwrap() - 4;
        assert_eq!(be32(&moof, second_trun + 8), 0x0000_0301);
        assert_eq!(be32(&moof, second_trun + 16) as usize, moof.len() + 8 + 150);
    }

    #[test]
    fn oversized_offsets_are_rejected() {
        let big = [TrunSample {
            duration: 1,
            size: u32::MAX,
            flags: 0,
        }];
        let trafs = [TrafSpec {
            track_id: 1,
            base_media_decode_time: 0,
            per_sample_flags: false,
            samples: &big,
        }];
        assert!(build_moof(1, &trafs).is_none());
    }
}
