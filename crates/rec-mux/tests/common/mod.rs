//! Shared helpers: fixture loading, packet building and ffprobe/ffmpeg wrappers.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use rec_core::media::{
    AudioCodec, AudioFormat, TrackInfo, TrackKind, VIDEO_TRACK, VideoCodec, VideoFormat,
};
use rec_core::replay::Packet;
use rec_core::time::{HNS_PER_SEC, Hns};

/// A QPC-like absolute timestamp, as produced by a machine up for a few days.
pub const BASE: Hns = 123_456_789_000_000;
pub const FPS: i64 = 30;
/// Video encode latency: a frame reaches the buffer this long after its pts,
/// while audio arrives when its last sample has been captured.
pub const VIDEO_LATENCY: Hns = 1_000_000;

pub fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

// ---------------------------------------------------------------- video

/// Splits an Annex-B stream into NAL units (independent of the crate's own
/// splitter so the tests do not share its bugs).
fn nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (k, &s) in starts.iter().enumerate() {
        let mut e = starts.get(k + 1).map_or(data.len(), |n| n - 3);
        while e > s && data[e - 1] == 0 {
            e -= 1;
        }
        out.push(&data[s..e]);
    }
    out
}

pub struct VideoAu {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// Groups NALs into access units (single slice per picture: every VCL NAL
/// ends one) and detects IDR pictures.
pub fn split_access_units(codec: VideoCodec, stream: &[u8]) -> Vec<VideoAu> {
    let mut aus = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    for nal in nals(stream) {
        cur.extend_from_slice(&[0, 0, 0, 1]);
        cur.extend_from_slice(nal);
        let (is_vcl, is_idr) = match codec {
            VideoCodec::H264 => {
                let t = nal[0] & 0x1F;
                ((1..=5).contains(&t), t == 5)
            }
            VideoCodec::Hevc => {
                let t = (nal[0] >> 1) & 0x3F;
                (t < 32, (19..=20).contains(&t))
            }
            VideoCodec::Av1 => unreachable!("AV1 is split by temporal unit"),
        };
        if is_vcl {
            aus.push(VideoAu {
                data: std::mem::take(&mut cur),
                keyframe: is_idr,
            });
        }
    }
    assert!(cur.is_empty(), "trailing non-VCL NALs in fixture");
    aus
}

/// Splits an IVF file into temporal units. A unit is a keyframe when it
/// carries a sequence header OBU (the encoder repeats it on every keyframe).
pub fn av1_aus() -> Vec<VideoAu> {
    let ivf = fixture("av1_320x240_30fps.ivf");
    assert_eq!(&ivf[..4], b"DKIF");
    let mut pos = usize::from(u16::from_le_bytes([ivf[6], ivf[7]]));
    let mut aus = Vec::new();
    while pos + 12 <= ivf.len() {
        let size = u32::from_le_bytes(ivf[pos..pos + 4].try_into().unwrap()) as usize;
        let unit = &ivf[pos + 12..pos + 12 + size];
        pos += 12 + size;
        let mut keyframe = false;
        let mut at = 0;
        while at < unit.len() {
            let kind = (unit[at] >> 3) & 0x0F;
            let ext = usize::from(unit[at] & 4 != 0);
            assert!(unit[at] & 2 != 0, "fixture OBUs carry size fields");
            let mut len = 0usize;
            let mut used = 0;
            for (k, b) in unit[at + 1 + ext..].iter().enumerate() {
                len |= usize::from(b & 0x7F) << (7 * k);
                used = k + 1;
                if b & 0x80 == 0 {
                    break;
                }
            }
            keyframe |= kind == 1;
            at += 1 + ext + used + len;
        }
        aus.push(VideoAu {
            data: unit.to_vec(),
            keyframe,
        });
    }
    aus
}

pub fn video_info(codec: VideoCodec) -> TrackInfo {
    TrackInfo {
        id: VIDEO_TRACK,
        name: "Video".into(),
        kind: TrackKind::Video(VideoFormat {
            codec,
            width: 320,
            height: 240,
            fps: FPS as u32,
            color: rec_core::media::VideoColor::Sdr,
        }),
    }
}

pub fn h264_aus() -> Vec<VideoAu> {
    split_access_units(VideoCodec::H264, &fixture("h264_320x240_30fps.h264"))
}

pub fn hevc_aus() -> Vec<VideoAu> {
    split_access_units(VideoCodec::Hevc, &fixture("hevc_320x240_30fps.h265"))
}

/// Frame `i` of a 30 fps stream starting at `BASE`.
pub fn frame_pts(i: usize) -> Hns {
    BASE + (i as i64 * HNS_PER_SEC + FPS / 2) / FPS
}

pub fn video_packets(aus: &[VideoAu], segment: u32) -> Vec<Packet> {
    aus.iter()
        .enumerate()
        .map(|(i, au)| {
            let pts = frame_pts(i);
            Packet {
                track: VIDEO_TRACK,
                segment,
                pts,
                dts: pts,
                duration: HNS_PER_SEC / FPS,
                keyframe: au.keyframe,
                data: Arc::from(au.data.as_slice()),
            }
        })
        .collect()
}

// ---------------------------------------------------------------- audio

const SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

pub struct AudioClip {
    pub format: AudioFormat,
    /// Raw access units.
    pub frames: Vec<Vec<u8>>,
}

/// Parses an ADTS stream into raw AAC access units and an AudioSpecificConfig.
pub fn parse_adts(stream: &[u8]) -> AudioClip {
    let mut frames = Vec::new();
    let mut config = None;
    let mut pos = 0;
    while pos + 7 <= stream.len() {
        assert!(
            stream[pos] == 0xFF && stream[pos + 1] & 0xF0 == 0xF0,
            "bad ADTS sync"
        );
        let protection_absent = stream[pos + 1] & 1 == 1;
        let profile = (stream[pos + 2] >> 6) & 3;
        let sfi = (stream[pos + 2] >> 2) & 0xF;
        let channels = ((stream[pos + 2] & 1) << 2) | (stream[pos + 3] >> 6);
        let len = (usize::from(stream[pos + 3] & 3) << 11)
            | (usize::from(stream[pos + 4]) << 3)
            | usize::from(stream[pos + 5] >> 5);
        let header = if protection_absent { 7 } else { 9 };
        if config.is_none() {
            let aot = u16::from(profile) + 1;
            let asc = (aot << 11) | (u16::from(sfi) << 7) | (u16::from(channels) << 3);
            config = Some((SAMPLE_RATES[usize::from(sfi)], u16::from(channels), asc));
        }
        frames.push(stream[pos + header..pos + len].to_vec());
        pos += len;
    }
    let (sample_rate, channels, asc) = config.expect("no ADTS frames");
    AudioClip {
        format: AudioFormat {
            codec: AudioCodec::Aac,
            sample_rate,
            channels,
            codec_config: asc.to_be_bytes().to_vec(),
        },
        frames,
    }
}

pub fn stereo_48k() -> AudioClip {
    parse_adts(&fixture("aac_48k_stereo.aac"))
}

pub fn mono_44k() -> AudioClip {
    parse_adts(&fixture("aac_44k_mono.aac"))
}

pub fn audio_info(id: u8, name: &str, clip: &AudioClip) -> TrackInfo {
    TrackInfo {
        id,
        name: name.into(),
        kind: TrackKind::Audio(clip.format.clone()),
    }
}

/// Audio frame `i` starts `i * 1024 / rate` seconds after `start`.
pub fn audio_pts(start: Hns, rate: u32, i: usize) -> Hns {
    start + (i as i64 * 1024 * HNS_PER_SEC + i64::from(rate) / 2) / i64::from(rate)
}

pub fn audio_packets(id: u8, clip: &AudioClip, start: Hns, segment: u32) -> Vec<Packet> {
    let rate = clip.format.sample_rate;
    clip.frames
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let pts = audio_pts(start, rate, i);
            Packet {
                track: id,
                segment,
                pts,
                dts: pts,
                duration: 1024 * HNS_PER_SEC / i64::from(rate),
                keyframe: true,
                data: Arc::from(f.as_slice()),
            }
        })
        .collect()
}

/// Orders packets by the time they would reach the replay buffer: video after
/// its encode latency, audio when its last sample has been captured.
pub fn arrival_order(mut packets: Vec<Packet>) -> Vec<Packet> {
    let key = |p: &Packet| {
        if p.track == VIDEO_TRACK {
            p.pts + VIDEO_LATENCY
        } else {
            p.pts + p.duration
        }
    };
    packets.sort_by_key(key);
    packets
}

// ---------------------------------------------------------------- tools

/// True when `ffprobe` and `ffmpeg` run; prints a note and returns false
/// otherwise so tests can skip.
pub fn have_ffmpeg(test: &str) -> bool {
    let ok = |tool: &str| {
        Command::new(tool)
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if ok("ffprobe") && ok("ffmpeg") {
        true
    } else {
        eprintln!("NOTE: skipping ffmpeg checks in `{test}`: ffprobe/ffmpeg not found on PATH");
        false
    }
}

pub fn temp_path(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("rec-mux");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

pub fn save(name: &str, bytes: &[u8]) -> PathBuf {
    let path = temp_path(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// One `[STREAM]` section of ffprobe output.
#[derive(Debug, Default, Clone)]
pub struct Stream(pub HashMap<String, String>);

impl Stream {
    pub fn get(&self, key: &str) -> &str {
        self.0
            .get(key)
            .unwrap_or_else(|| panic!("missing {key} in {:?}", self.0))
    }
    pub fn f64(&self, key: &str) -> f64 {
        self.get(key)
            .parse()
            .unwrap_or_else(|_| panic!("{key}={} is not a number", self.get(key)))
    }
    pub fn usize(&self, key: &str) -> usize {
        self.get(key).parse().unwrap()
    }
}

#[derive(Debug)]
pub struct Probe {
    pub streams: Vec<Stream>,
    pub format: Stream,
}

impl Probe {
    pub fn video(&self) -> &Stream {
        self.streams
            .iter()
            .find(|s| s.get("codec_type") == "video")
            .expect("no video stream")
    }
    pub fn audio(&self) -> Vec<&Stream> {
        self.streams
            .iter()
            .filter(|s| s.get("codec_type") == "audio")
            .collect()
    }
}

/// Runs `ffprobe -count_frames` and parses its default key=value output.
pub fn probe(path: &Path) -> Probe {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_frames",
            "-show_streams",
            "-show_format",
        ])
        .arg(path)
        .output()
        .expect("running ffprobe");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success() && out.stderr.is_empty(),
        "ffprobe failed on {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut streams = Vec::new();
    let mut format = Stream::default();
    let mut current: Option<Stream> = None;
    let mut in_format = false;
    for line in text.lines() {
        match line.trim() {
            "[STREAM]" => current = Some(Stream::default()),
            "[/STREAM]" => streams.extend(current.take()),
            "[FORMAT]" => in_format = true,
            "[/FORMAT]" => in_format = false,
            l => {
                if let Some((k, v)) = l.split_once('=') {
                    if let Some(c) = current.as_mut() {
                        c.0.insert(k.to_owned(), v.to_owned());
                    } else if in_format {
                        format.0.insert(k.to_owned(), v.to_owned());
                    }
                }
            }
        }
    }
    Probe { streams, format }
}

/// Decodes everything to a null sink and returns what ffmpeg printed at
/// `-v error` level (empty when the file is clean).
pub fn decode_errors(path: &Path) -> String {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-xerror", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
        .expect("running ffmpeg");
    let mut text = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() {
        text.push_str("\n(ffmpeg exited with failure)");
    }
    text
}

/// Number of video frames ffprobe decodes from the first video stream;
/// tolerant of truncated files (returns 0 when nothing can be read).
pub fn decoded_video_frames(path: &Path) -> usize {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-count_frames",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=nb_read_frames",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("running ffprobe");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

// ------------------------------------------------------------ box walking

/// A top-level box: `(fourcc, offset, size)`.
pub fn top_level_boxes(file: &[u8]) -> Vec<([u8; 4], usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 8 <= file.len() {
        let size = u32::from_be_bytes(file[pos..pos + 4].try_into().unwrap()) as usize;
        assert!(size >= 8, "bad box size at {pos}");
        let cc: [u8; 4] = file[pos + 4..pos + 8].try_into().unwrap();
        out.push((cc, pos, size.min(file.len() - pos)));
        pos += size;
    }
    out
}

pub fn offsets_of(file: &[u8], fourcc: &[u8; 4]) -> Vec<usize> {
    top_level_boxes(file)
        .into_iter()
        .filter(|b| &b.0 == fourcc)
        .map(|b| b.1)
        .collect()
}
