//! The initialization segment: `ftyp` and an empty-sample-table `moov` with
//! `mvex`. Duration fields are written as zero and patched by the writer when
//! the clip is finished, so the positions of those fields are reported back.

use crate::boxes::{Put, UNITY_MATRIX, boxed, full_box};
use crate::error::MuxError;
use crate::video::CodecConfig;

pub(crate) const MOVIE_TIMESCALE: u32 = 1000;

/// trex default sample flags: sync sample / non-sync sample.
pub(crate) const FLAGS_SYNC: u32 = 0x0200_0000;
pub(crate) const FLAGS_NON_SYNC: u32 = 0x0101_0000;

pub(crate) enum TrackKind<'a> {
    Video {
        width: u16,
        height: u16,
        config: &'a CodecConfig,
    },
    Audio {
        channels: u16,
        sample_rate: u32,
        audio_specific_config: &'a [u8],
    },
}

pub(crate) struct InitTrack<'a> {
    pub track_id: u32,
    pub name: &'a str,
    pub timescale: u32,
    pub enabled: bool,
    pub kind: TrackKind<'a>,
}

/// Where the duration fields live in the init segment (file offsets, because
/// the init segment starts at offset 0).
#[derive(Clone, Debug)]
pub(crate) struct Patches {
    pub mvhd_duration: u64,
    pub mehd_duration: u64,
    pub tracks: Vec<TrackPatches>,
}

#[derive(Clone, Debug)]
pub(crate) struct TrackPatches {
    pub tkhd_duration: u64,
    pub mdhd_duration: u64,
}

pub(crate) fn build_init(
    tracks: &[InitTrack<'_>],
    creation_time: u32,
) -> Result<(Vec<u8>, Patches), MuxError> {
    let mut buf = Vec::with_capacity(2048);
    boxed(&mut buf, b"ftyp", |b| {
        b.put_fourcc(b"isom");
        b.put_u32(0x200);
        for brand in [b"isom", b"iso5", b"iso6", b"mp41"] {
            b.put_fourcc(brand);
        }
    });

    let mut patches = Patches {
        mvhd_duration: 0,
        mehd_duration: 0,
        tracks: Vec::new(),
    };
    let mut error = None;
    boxed(&mut buf, b"moov", |b| {
        full_box(b, b"mvhd", 0, 0, |b| {
            b.put_u32(creation_time);
            b.put_u32(creation_time);
            b.put_u32(MOVIE_TIMESCALE);
            patches.mvhd_duration = b.len() as u64;
            b.put_u32(0); // duration, patched
            b.put_u32(0x0001_0000); // rate 1.0
            b.put_u16(0x0100); // volume 1.0
            b.put_zeros(10);
            for m in UNITY_MATRIX {
                b.put_u32(m);
            }
            b.put_zeros(24); // pre_defined
            b.put_u32(tracks.len() as u32 + 1); // next_track_ID
        });
        for t in tracks {
            match trak(b, t, creation_time) {
                Ok(p) => patches.tracks.push(p),
                Err(e) => {
                    error = Some(e);
                    return;
                }
            }
        }
        boxed(b, b"mvex", |b| {
            full_box(b, b"mehd", 0, 0, |b| {
                patches.mehd_duration = b.len() as u64;
                b.put_u32(0); // fragment_duration, patched
            });
            for t in tracks {
                full_box(b, b"trex", 0, 0, |b| {
                    b.put_u32(t.track_id);
                    b.put_u32(1); // default_sample_description_index
                    b.put_u32(0); // default_sample_duration
                    b.put_u32(0); // default_sample_size
                    b.put_u32(match t.kind {
                        TrackKind::Video { .. } => FLAGS_NON_SYNC,
                        TrackKind::Audio { .. } => FLAGS_SYNC,
                    });
                });
            }
        });
    });
    match error {
        Some(e) => Err(e),
        None => Ok((buf, patches)),
    }
}

fn trak(b: &mut Vec<u8>, t: &InitTrack<'_>, creation_time: u32) -> Result<TrackPatches, MuxError> {
    let mut p = TrackPatches {
        tkhd_duration: 0,
        mdhd_duration: 0,
    };
    let is_video = matches!(t.kind, TrackKind::Video { .. });
    let mut error = None;
    boxed(b, b"trak", |b| {
        // flags: 1 = enabled, 2 = in movie
        full_box(b, b"tkhd", 0, if t.enabled { 3 } else { 2 }, |b| {
            b.put_u32(creation_time);
            b.put_u32(creation_time);
            b.put_u32(t.track_id);
            b.put_u32(0);
            p.tkhd_duration = b.len() as u64;
            b.put_u32(0); // duration, patched
            b.put_zeros(8);
            b.put_u16(0); // layer
            b.put_u16(if is_video { 0 } else { 1 }); // alternate group: audio tracks are alternates
            b.put_u16(if is_video { 0 } else { 0x0100 });
            b.put_u16(0);
            for m in UNITY_MATRIX {
                b.put_u32(m);
            }
            match t.kind {
                TrackKind::Video { width, height, .. } => {
                    b.put_u32(u32::from(width) << 16);
                    b.put_u32(u32::from(height) << 16);
                }
                TrackKind::Audio { .. } => {
                    b.put_u32(0);
                    b.put_u32(0);
                }
            }
        });
        boxed(b, b"mdia", |b| {
            full_box(b, b"mdhd", 0, 0, |b| {
                b.put_u32(creation_time);
                b.put_u32(creation_time);
                b.put_u32(t.timescale);
                p.mdhd_duration = b.len() as u64;
                b.put_u32(0); // duration, patched
                b.put_u16(0x55C4); // language "und"
                b.put_u16(0);
            });
            full_box(b, b"hdlr", 0, 0, |b| {
                b.put_u32(0);
                b.put_fourcc(if is_video { b"vide" } else { b"soun" });
                b.put_zeros(12);
                b.extend_from_slice(t.name.as_bytes());
                b.put_u8(0);
            });
            boxed(b, b"minf", |b| {
                match t.kind {
                    TrackKind::Video { .. } => full_box(b, b"vmhd", 0, 1, |b| b.put_zeros(8)),
                    TrackKind::Audio { .. } => full_box(b, b"smhd", 0, 0, |b| b.put_zeros(4)),
                }
                boxed(b, b"dinf", |b| {
                    full_box(b, b"dref", 0, 0, |b| {
                        b.put_u32(1);
                        full_box(b, b"url ", 0, 1, |_| {});
                    });
                });
                boxed(b, b"stbl", |b| {
                    full_box(b, b"stsd", 0, 0, |b| {
                        b.put_u32(1);
                        if let Err(e) = sample_entry(b, t) {
                            error = Some(e);
                        }
                    });
                    full_box(b, b"stts", 0, 0, |b| b.put_u32(0));
                    full_box(b, b"stsc", 0, 0, |b| b.put_u32(0));
                    full_box(b, b"stsz", 0, 0, |b| b.put_zeros(8));
                    full_box(b, b"stco", 0, 0, |b| b.put_u32(0));
                });
            });
        });
    });
    match error {
        Some(e) => Err(e),
        None => Ok(p),
    }
}

fn sample_entry(b: &mut Vec<u8>, t: &InitTrack<'_>) -> Result<(), MuxError> {
    match &t.kind {
        TrackKind::Video {
            width,
            height,
            config,
        } => {
            boxed(b, &config.entry, |b| {
                b.put_zeros(6);
                b.put_u16(1); // data_reference_index
                b.put_zeros(16); // pre_defined / reserved
                b.put_u16(*width);
                b.put_u16(*height);
                b.put_u32(0x0048_0000); // 72 dpi
                b.put_u32(0x0048_0000);
                b.put_u32(0);
                b.put_u16(1); // frame_count
                b.put_zeros(32); // compressorname
                b.put_u16(0x0018); // depth
                b.put_u16(0xFFFF); // pre_defined = -1
                boxed(b, &config.config_box, |b| {
                    b.extend_from_slice(&config.config)
                });
            });
            Ok(())
        }
        TrackKind::Audio {
            channels,
            sample_rate,
            audio_specific_config,
        } => {
            if audio_specific_config.is_empty() {
                return Err(MuxError::InvalidTracks(
                    "audio track has an empty codec_config".into(),
                ));
            }
            boxed(b, b"mp4a", |b| {
                b.put_zeros(6);
                b.put_u16(1);
                b.put_zeros(8);
                b.put_u16(*channels);
                b.put_u16(16);
                b.put_u16(0);
                b.put_u16(0);
                // 16.16 fixed point; rates above 65535 do not fit and are
                // given by the AudioSpecificConfig and mdhd instead.
                b.put_u32(if *sample_rate <= 0xFFFF {
                    sample_rate << 16
                } else {
                    0
                });
                full_box(b, b"esds", 0, 0, |b| {
                    descriptor(b, 0x03, |b| {
                        b.put_u16(t.track_id as u16);
                        b.put_u8(0);
                        descriptor(b, 0x04, |b| {
                            b.put_u8(0x40); // MPEG-4 audio
                            b.put_u8(0x15); // audio stream
                            b.extend_from_slice(&[0, 0, 0]); // bufferSizeDB
                            b.put_u32(0); // maxBitrate
                            b.put_u32(0); // avgBitrate
                            descriptor(b, 0x05, |b| b.extend_from_slice(audio_specific_config));
                        });
                        descriptor(b, 0x06, |b| b.put_u8(0x02));
                    });
                });
            });
            Ok(())
        }
    }
}

/// MPEG-4 descriptor with the 4-byte padded length form.
fn descriptor(b: &mut Vec<u8>, tag: u8, body: impl FnOnce(&mut Vec<u8>)) {
    b.put_u8(tag);
    let len_pos = b.len();
    b.put_zeros(4);
    body(b);
    let len = (b.len() - len_pos - 4) as u32;
    b[len_pos] = 0x80 | ((len >> 21) & 0x7F) as u8;
    b[len_pos + 1] = 0x80 | ((len >> 14) & 0x7F) as u8;
    b[len_pos + 2] = 0x80 | ((len >> 7) & 0x7F) as u8;
    b[len_pos + 3] = (len & 0x7F) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(buf: &[u8], fourcc: &[u8; 4]) -> Option<usize> {
        buf.windows(4).position(|w| w == fourcc).map(|p| p - 4)
    }

    #[test]
    fn descriptor_length_uses_four_bytes() {
        let mut b = Vec::new();
        descriptor(&mut b, 0x05, |b| b.extend_from_slice(&[1, 2]));
        assert_eq!(b, [0x05, 0x80, 0x80, 0x80, 2, 1, 2]);
        let mut b = Vec::new();
        descriptor(&mut b, 0x03, |b| b.put_zeros(300));
        assert_eq!(&b[..5], [0x03, 0x80, 0x80, 0x82, 0x2C]);
    }

    #[test]
    fn init_reports_patch_positions_that_hold_zero() {
        let cfg = CodecConfig {
            entry: *b"avc1",
            config_box: *b"avcC",
            config: vec![1, 2, 3],
        };
        let asc = [0x11, 0x90];
        let tracks = [
            InitTrack {
                track_id: 1,
                name: "Video",
                timescale: 90_000,
                enabled: true,
                kind: TrackKind::Video {
                    width: 320,
                    height: 240,
                    config: &cfg,
                },
            },
            InitTrack {
                track_id: 2,
                name: "Game",
                timescale: 48_000,
                enabled: true,
                kind: TrackKind::Audio {
                    channels: 2,
                    sample_rate: 48_000,
                    audio_specific_config: &asc,
                },
            },
        ];
        let (init, patches) = build_init(&tracks, 0).unwrap();
        assert_eq!(&init[4..8], b"ftyp");
        assert!(find(&init, b"moov").is_some());
        assert!(find(&init, b"avcC").is_some());
        assert!(find(&init, b"esds").is_some());
        assert!(find(&init, b"mehd").is_some());
        let at =
            |pos: u64| u32::from_be_bytes(init[pos as usize..pos as usize + 4].try_into().unwrap());
        assert_eq!(at(patches.mvhd_duration), 0);
        assert_eq!(at(patches.mehd_duration), 0);
        assert_eq!(patches.tracks.len(), 2);
        let mdhd = find(&init, b"mdhd").unwrap();
        assert_eq!(at(patches.tracks[0].mdhd_duration), 0);
        assert!(patches.tracks[0].mdhd_duration as usize > mdhd);
        // The first mdhd carries the video timescale just before the duration.
        assert_eq!(at(patches.tracks[0].mdhd_duration - 4), 90_000);
        assert_eq!(at(patches.tracks[1].mdhd_duration - 4), 48_000);
        // Top-level box sizes add up.
        let ftyp = u32::from_be_bytes(init[0..4].try_into().unwrap()) as usize;
        let moov = u32::from_be_bytes(init[ftyp..ftyp + 4].try_into().unwrap()) as usize;
        assert_eq!(ftyp + moov, init.len());
    }

    #[test]
    fn empty_audio_config_is_rejected() {
        let tracks = [InitTrack {
            track_id: 1,
            name: "Game",
            timescale: 48_000,
            enabled: true,
            kind: TrackKind::Audio {
                channels: 2,
                sample_rate: 48_000,
                audio_specific_config: &[],
            },
        }];
        assert!(build_init(&tracks, 0).is_err());
    }
}
