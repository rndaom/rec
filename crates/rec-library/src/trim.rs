//! Lossless trimming of rec clips.
//!
//! rec-mux writes one `moof`+`mdat` fragment per GOP, each starting with a
//! video keyframe (see the layout in the `rec_mux` crate docs), so a clip is
//! cut between fragments without touching the bitstream: H.264, HEVC, AV1
//! and the HDR boxes come through byte for byte. The start snaps back to the
//! keyframe that opens its fragment (an IDR every second); the end is exact
//! to the frame, by shortening the last kept fragment's `trun`s. Times are
//! rebased so the new clip starts at 0, audio keeping its offset from the
//! keyframe through `tfdt` alone, as rec-mux does.
//!
//! Only rec's own layout is accepted (`default-base-is-moof`, one `trun` per
//! `traf`, a keyframe first in every fragment): anything else is refused
//! rather than half understood.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::clip::{Clip, round3, sidecar_for, write_sidecar};
use crate::time::{format_utc, parse_utc};

/// Shortest clip a trim may produce, after the start snaps to its keyframe.
pub const MIN_DURATION: f64 = 0.5;

/// Slack when comparing a start with a keyframe time, so a start typed from a
/// rounded display (`3.000` for 2.99997) still lands on that keyframe.
const START_SLACK: f64 = 0.0005;

/// `tfhd` flags.
const TFHD_BASE_DATA_OFFSET: u32 = 0x01;
const TFHD_DESCRIPTION_INDEX: u32 = 0x02;
const TFHD_DURATION: u32 = 0x08;
const TFHD_SIZE: u32 = 0x10;
const TFHD_FLAGS: u32 = 0x20;
const TFHD_DURATION_IS_EMPTY: u32 = 0x01_0000;
const TFHD_BASE_IS_MOOF: u32 = 0x02_0000;

/// `trun` flags.
const TRUN_DATA_OFFSET: u32 = 0x001;
const TRUN_FIRST_FLAGS: u32 = 0x004;
const TRUN_DURATION: u32 = 0x100;
const TRUN_SIZE: u32 = 0x200;
const TRUN_FLAGS: u32 = 0x400;
const TRUN_CTO: u32 = 0x800;

/// `sample_is_non_sync_sample` in sample flags.
const NON_SYNC: u32 = 0x0001_0000;

/// A finished trim.
#[derive(Clone, Debug, PartialEq)]
pub struct Trimmed {
    pub video: PathBuf,
    /// The new clip's sidecar, when one was written.
    pub sidecar: Option<PathBuf>,
    /// The actual cut, in the source clip's time: `start` is the keyframe the
    /// requested start snapped back to, `end` the end of the last kept frame.
    pub start: f64,
    pub end: f64,
}

/// Where a trim of `path` from `start` to `end` (clip seconds) would cut,
/// without writing anything: the UI shows where the start snaps.
pub fn plan(path: &Path, start: f64, end: f64) -> Result<(f64, f64), String> {
    let mut file = File::open(path).map_err(|e| format!("can't open {}: {e}", path.display()))?;
    let layout = Layout::read(&mut file).map_err(|e| format!("{}: {e}", path.display()))?;
    let cut = layout.cut(start, end)?;
    Ok((cut.start_secs(), cut.end_secs()))
}

/// Trims `clip` to `start`..`end` (clip seconds) without re-encoding. The
/// new video goes to `out`, or next to the clip as `<stem> trim.mp4` (then
/// `<stem> trim 2.mp4`, ...). The clip's sidecar, or the one its journal line
/// implies, is carried over with times shifted and `parent` set to the clip.
pub fn trim(clip: &Clip, start: f64, end: f64, out: Option<&Path>) -> Result<Trimmed, String> {
    let source = clip.video.as_deref().ok_or("the clip's video is missing")?;
    let mut file =
        File::open(source).map_err(|e| format!("can't open {}: {e}", source.display()))?;
    let layout = Layout::read(&mut file).map_err(|e| format!("{}: {e}", source.display()))?;
    let cut = layout.cut(start, end)?;

    let out = match out {
        Some(out) => {
            if same_file(out, source) {
                return Err("the trim can't replace the clip it is cut from".into());
            }
            out.to_owned()
        }
        None => default_output(source),
    };
    let name = (out.file_name())
        .ok_or_else(|| format!("{} is not a file name", out.display()))?
        .to_string_lossy()
        .into_owned();
    let tmp = out.with_file_name(format!("{name}.tmp"));
    let written = File::create(&tmp)
        .and_then(|f| {
            let mut w = BufWriter::new(f);
            layout.write(&cut, &mut file, &mut w)?;
            w.into_inner()
                .map_err(io::IntoInnerError::into_error)?
                .sync_all()
        })
        .and_then(|()| fs::rename(&tmp, &out));
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(format!("can't write {}: {e}", out.display()));
    }

    let (start, end) = (cut.start_secs(), cut.end_secs());
    let sidecar = match trimmed_sidecar(clip, &name, start, end) {
        Some(raw) => {
            let path = sidecar_for(&out);
            if let Err(e) = write_sidecar(&path, &raw) {
                let _ = fs::remove_file(&out);
                return Err(e);
            }
            Some(path)
        }
        None => None,
    };
    Ok(Trimmed {
        video: out,
        sidecar,
        start,
        end,
    })
}

/// `<stem> trim.mp4` next to `video`, numbered when that name (or its
/// sidecar) is taken.
fn default_output(video: &Path) -> PathBuf {
    let stem = video
        .file_stem()
        .map_or_else(|| "clip".into(), |s| s.to_string_lossy().into_owned());
    (1..)
        .map(|n| match n {
            1 => video.with_file_name(format!("{stem} trim.mp4")),
            n => video.with_file_name(format!("{stem} trim {n}.mp4")),
        })
        .find(|p| !p.exists() && !sidecar_for(p).exists())
        .expect("an unused name")
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The sidecar of a trim from `start` to `end`: the clip's own, unknown
/// fields kept, with a new id, `parent` set, and every clip time shifted.
/// Stale facts go: `analysis` (rerun it), `video.dropped_frames` (counted
/// over the whole clip) and the star. `None` if the clip has no sidecar
/// shape at all.
pub(crate) fn trimmed_sidecar(clip: &Clip, file: &str, start: f64, end: f64) -> Option<Value> {
    let mut raw = clip.raw.clone();
    let fields = raw.as_object_mut()?;
    if !fields.get("clip").is_some_and(Value::is_object) {
        return None;
    }
    // A journal-only clip's facts become a real sidecar for the trim.
    fields.shift_remove("source");
    fields.shift_remove("analysis");
    if fields.contains_key("starred") {
        fields.insert("starred".into(), false.into());
    }
    if let Some(video) = fields.get_mut("video").and_then(Value::as_object_mut) {
        video.shift_remove("dropped_frames");
    }

    let duration = end - start;
    let c = &mut raw["clip"];
    c["id"] = rec_core::store::new_id().into();
    c["file"] = file.into();
    c["parent"] = clip.id.clone().into();
    c["duration"] = round3(duration).into();
    if let Some(saved) = c.get("saved_at").and_then(Value::as_f64) {
        let t = saved - start;
        c["saved_at"] = if (0.0..=duration).contains(&t) {
            round3(t).into()
        } else {
            Value::Null
        };
    }

    if let Some(time) = raw.get_mut("time").filter(|t| t.is_object()) {
        if let Some(utc) = time["start_utc"].as_str().and_then(parse_utc) {
            time["start_utc"] = format_utc(utc + start).into();
        }
        if let Some(offset) = time["session_offset"].as_f64() {
            time["session_offset"] = round3(offset + start).into();
        }
    }

    if let Some(events) = raw.get_mut("events").and_then(Value::as_array_mut) {
        events.retain_mut(|e| shift_event(e, start, end));
    }

    let hz = f64::from(clip.hz);
    for track in (raw["audio"].as_array_mut().into_iter()).flatten() {
        if let Some(curve) = track.get_mut("loudness").and_then(Value::as_array_mut) {
            let at = |t: f64| ((t * hz).round().max(0.0) as usize).min(curve.len());
            let (lo, hi) = (at(start), at(end));
            curve.truncate(hi);
            curve.drain(..lo.min(hi));
        }
    }
    Some(raw)
}

/// Moves an event into the trim's time; `false` drops it. A point event is
/// kept inside `start..=end`; a ranged one (`dur`) while it overlaps, clipped.
fn shift_event(e: &mut Value, start: f64, end: f64) -> bool {
    let Some(t) = e["t"].as_f64() else {
        return false;
    };
    match e["dur"].as_f64().filter(|d| *d > 0.0) {
        Some(dur) => {
            if t + dur <= start || t >= end {
                return false;
            }
            let (from, to) = (t.max(start), (t + dur).min(end));
            e["t"] = round3(from - start).into();
            e["dur"] = round3(to - from).into();
        }
        None => {
            if t < start || t > end {
                return false;
            }
            e["t"] = round3(t - start).into();
        }
    }
    true
}

// ---- reading ------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default)]
struct Defaults {
    duration: u32,
    size: u32,
    flags: u32,
}

/// A duration field inside `moov`: its offset in the buffer and width.
#[derive(Clone, Copy, Debug)]
struct Field {
    at: usize,
    wide: bool,
}

#[derive(Debug)]
struct Track {
    id: u32,
    timescale: u32,
    video: bool,
    trex: Defaults,
    tkhd: Field,
    mdhd: Field,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sample {
    duration: u32,
    size: u32,
    flags: u32,
    cto: u32,
}

#[derive(Debug)]
struct Traf {
    /// Index into [`Layout::tracks`].
    track: usize,
    tfhd_flags: u32,
    tfhd: Defaults,
    description_index: u32,
    tfdt: u64,
    trun_version: u8,
    trun_flags: u32,
    samples: Vec<Sample>,
    /// File offset of the first sample's data.
    data: u64,
}

impl Traf {
    fn bytes_before(&self, sample: usize) -> u64 {
        self.samples[..sample]
            .iter()
            .map(|s| u64::from(s.size))
            .sum()
    }

    /// Start time of each sample, in track ticks.
    fn times(&self) -> impl Iterator<Item = u64> + '_ {
        self.samples.iter().scan(self.tfdt, |t, s| {
            let at = *t;
            *t += u64::from(s.duration);
            Some(at)
        })
    }

    fn end(&self) -> u64 {
        self.tfdt
            + self
                .samples
                .iter()
                .map(|s| u64::from(s.duration))
                .sum::<u64>()
    }
}

#[derive(Debug)]
struct Fragment {
    trafs: Vec<Traf>,
}

impl Fragment {
    fn video(&self, layout: &Layout) -> &Traf {
        (self.trafs.iter())
            .find(|t| t.track == layout.video)
            .expect("checked when read")
    }
}

/// A rec clip's boxes: the init segment in memory, the fragments parsed.
#[derive(Debug)]
struct Layout {
    ftyp: Vec<u8>,
    moov: Vec<u8>,
    movie_timescale: u32,
    mvhd: Field,
    mehd: Option<Field>,
    tracks: Vec<Track>,
    /// Index of the video track.
    video: usize,
    fragments: Vec<Fragment>,
}

/// A box inside a buffer: its type and where its body and end are.
#[derive(Clone, Copy, Debug)]
struct Child {
    kind: [u8; 4],
    body: usize,
    end: usize,
}

fn short() -> String {
    "not a rec clip: a box is cut short".into()
}

fn be32(b: &[u8], at: usize) -> Result<u32, String> {
    let s = b.get(at..at + 4).ok_or_else(short)?;
    Ok(u32::from_be_bytes(s.try_into().expect("4 bytes")))
}

fn be64(b: &[u8], at: usize) -> Result<u64, String> {
    let s = b.get(at..at + 8).ok_or_else(short)?;
    Ok(u64::from_be_bytes(s.try_into().expect("8 bytes")))
}

/// The boxes in `buf[from..to]`.
fn children(buf: &[u8], from: usize, to: usize) -> Result<Vec<Child>, String> {
    let mut out = Vec::new();
    let mut at = from;
    while at + 8 <= to {
        let size = be32(buf, at)?;
        let kind: [u8; 4] = buf[at + 4..at + 8].try_into().expect("4 bytes");
        let (size, header) = match size {
            0 => ((to - at) as u64, 8),
            1 => (be64(buf, at + 8)?, 16),
            n => (u64::from(n), 8),
        };
        let end = usize::try_from(size)
            .ok()
            .and_then(|s| at.checked_add(s))
            .filter(|&e| e <= to && size >= header as u64)
            .ok_or_else(short)?;
        out.push(Child {
            kind,
            body: at + header,
            end,
        });
        at = end;
    }
    Ok(out)
}

fn find<'a>(boxes: &'a [Child], kind: &[u8; 4]) -> Option<&'a Child> {
    boxes.iter().find(|c| &c.kind == kind)
}

fn need<'a>(boxes: &'a [Child], kind: &[u8; 4]) -> Result<&'a Child, String> {
    find(boxes, kind).ok_or_else(|| {
        format!(
            "not a rec clip: no `{}` box",
            String::from_utf8_lossy(kind).trim_end()
        )
    })
}

/// A top-level box in the file.
#[derive(Clone, Copy, Debug)]
struct Top {
    kind: [u8; 4],
    start: u64,
    body: u64,
    end: u64,
}

/// The file's top-level boxes. A box that runs past the end of the file (a
/// clip cut off by a crash) ends the list: everything before it plays.
fn top_level(file: &mut File) -> io::Result<Vec<Top>> {
    let len = file.seek(SeekFrom::End(0))?;
    let mut out = Vec::new();
    let mut at = 0u64;
    while at + 8 <= len {
        let mut h = [0u8; 16];
        file.seek(SeekFrom::Start(at))?;
        file.read_exact(&mut h[..8])?;
        let kind: [u8; 4] = h[4..8].try_into().expect("4 bytes");
        let (size, header) = match u32::from_be_bytes(h[..4].try_into().expect("4 bytes")) {
            0 => (len - at, 8),
            1 => {
                if at + 16 > len {
                    break;
                }
                file.read_exact(&mut h[8..16])?;
                (
                    u64::from_be_bytes(h[8..16].try_into().expect("8 bytes")),
                    16,
                )
            }
            n => (u64::from(n), 8),
        };
        if size < header || at.saturating_add(size) > len {
            break;
        }
        out.push(Top {
            kind,
            start: at,
            body: at + header,
            end: at + size,
        });
        at += size;
    }
    Ok(out)
}

fn read_box(file: &mut File, b: &Top) -> Result<Vec<u8>, String> {
    let len = usize::try_from(b.end - b.start).map_err(|_| short())?;
    if len > 64 << 20 {
        return Err(format!(
            "not a rec clip: a `{}` box of {len} bytes",
            String::from_utf8_lossy(&b.kind)
        ));
    }
    let mut buf = vec![0; len];
    file.seek(SeekFrom::Start(b.start))
        .and_then(|_| file.read_exact(&mut buf))
        .map_err(|e| format!("can't read: {e}"))?;
    Ok(buf)
}

impl Layout {
    fn read(file: &mut File) -> Result<Self, String> {
        let top = top_level(file).map_err(|e| format!("can't read: {e}"))?;
        let not_mp4 = || String::from("not a rec clip: not an MP4 file");
        let ftyp = top
            .first()
            .filter(|b| &b.kind == b"ftyp")
            .ok_or_else(not_mp4)?;
        let ftyp = read_box(file, ftyp)?;
        let moov = top
            .iter()
            .find(|b| &b.kind == b"moov")
            .ok_or_else(not_mp4)?;
        let moov = read_box(file, moov)?;
        let mut layout = Self::parse_moov(ftyp, moov)?;

        let mut i = 0;
        while i < top.len() {
            let b = top[i];
            i += 1;
            if &b.kind != b"moof" {
                continue;
            }
            // A moof whose mdat was cut off is dropped like the rest of the tail.
            let Some(mdat) = top.get(i).filter(|m| &m.kind == b"mdat") else {
                if i == top.len() {
                    break;
                }
                return Err("not a rec clip: a `moof` isn't followed by its `mdat`".into());
            };
            i += 1;
            let moof = read_box(file, &b)?;
            let fragment = layout.parse_moof(&moof, b.start, (mdat.body, mdat.end))?;
            layout.fragments.push(fragment);
        }
        if layout.fragments.is_empty() {
            return Err("not a rec clip: no fragments".into());
        }
        let mut previous = None;
        for f in &layout.fragments {
            let start = f.video(&layout).tfdt;
            if previous.is_some_and(|p| start < p) {
                return Err("not a rec clip: fragments out of order".into());
            }
            previous = Some(f.video(&layout).end());
        }
        Ok(layout)
    }

    fn parse_moov(ftyp: Vec<u8>, moov: Vec<u8>) -> Result<Self, String> {
        let top = children(&moov, 8, moov.len())?;
        let mvhd = need(&top, b"mvhd")?;
        let wide = moov[mvhd.body] == 1;
        let (timescale_at, duration_at) = if wide { (20, 24) } else { (12, 16) };
        let movie_timescale = be32(&moov, mvhd.body + timescale_at)?;
        let mvhd = Field {
            at: mvhd.body + duration_at,
            wide,
        };
        be32(&moov, mvhd.at)?;
        let mvex = find(&top, b"mvex").ok_or("not a rec clip: not a fragmented MP4")?;
        let mvex = children(&moov, mvex.body, mvex.end)?;
        let mehd = find(&mvex, b"mehd").map(|b| Field {
            at: b.body + 4,
            wide: moov[b.body] == 1,
        });

        let mut tracks = Vec::new();
        for trak in top.iter().filter(|c| &c.kind == b"trak") {
            let boxes = children(&moov, trak.body, trak.end)?;
            let tkhd = need(&boxes, b"tkhd")?;
            let tkhd_wide = moov[tkhd.body] == 1;
            let id = be32(&moov, tkhd.body + if tkhd_wide { 20 } else { 12 })?;
            let tkhd = Field {
                at: tkhd.body + if tkhd_wide { 28 } else { 20 },
                wide: tkhd_wide,
            };
            let mdia = need(&boxes, b"mdia")?;
            let mdia = children(&moov, mdia.body, mdia.end)?;
            let mdhd = need(&mdia, b"mdhd")?;
            let mdhd_wide = moov[mdhd.body] == 1;
            let timescale = be32(&moov, mdhd.body + if mdhd_wide { 20 } else { 12 })?;
            let mdhd = Field {
                at: mdhd.body + if mdhd_wide { 24 } else { 16 },
                wide: mdhd_wide,
            };
            let hdlr = need(&mdia, b"hdlr")?;
            let video = moov.get(hdlr.body + 8..hdlr.body + 12) == Some(b"vide");
            let trex = (mvex.iter())
                .filter(|c| &c.kind == b"trex")
                .find(|c| be32(&moov, c.body + 4).ok() == Some(id))
                .ok_or("not a rec clip: a track has no `trex`")?;
            if timescale == 0 {
                return Err("not a rec clip: a track has no timescale".into());
            }
            tracks.push(Track {
                id,
                timescale,
                video,
                trex: Defaults {
                    duration: be32(&moov, trex.body + 12)?,
                    size: be32(&moov, trex.body + 16)?,
                    flags: be32(&moov, trex.body + 20)?,
                },
                tkhd,
                mdhd,
            });
        }
        let mut videos = tracks.iter().enumerate().filter(|(_, t)| t.video);
        let video = match (videos.next(), videos.next()) {
            (Some((i, _)), None) => i,
            _ => return Err("not a rec clip: it needs exactly one video track".into()),
        };
        if movie_timescale == 0 {
            return Err("not a rec clip: no movie timescale".into());
        }
        Ok(Self {
            ftyp,
            moov,
            movie_timescale,
            mvhd,
            mehd,
            tracks,
            video,
            fragments: Vec::new(),
        })
    }

    /// Parses one `moof`; `at` is its file offset and `mdat` the payload range
    /// of the `mdat` after it, which every sample must lie in.
    fn parse_moof(&self, moof: &[u8], at: u64, mdat: (u64, u64)) -> Result<Fragment, String> {
        let mut trafs = Vec::new();
        for traf in children(moof, 8, moof.len())?
            .iter()
            .filter(|c| &c.kind == b"traf")
        {
            let boxes = children(moof, traf.body, traf.end)?;
            let tfhd = need(&boxes, b"tfhd")?;
            let flags = be32(moof, tfhd.body)? & 0x00FF_FFFF;
            let id = be32(moof, tfhd.body + 4)?;
            let track = (self.tracks.iter())
                .position(|t| t.id == id)
                .ok_or("not a rec clip: a fragment names an unknown track")?;
            if flags & TFHD_DURATION_IS_EMPTY != 0 {
                continue;
            }
            if flags & TFHD_BASE_DATA_OFFSET != 0 || flags & TFHD_BASE_IS_MOOF == 0 {
                return Err("not a rec clip: sample data isn't addressed from the `moof`".into());
            }
            let mut p = tfhd.body + 8;
            let mut take = |flag: u32| -> Result<Option<u32>, String> {
                if flags & flag == 0 {
                    return Ok(None);
                }
                let v = be32(moof, p)?;
                p += 4;
                Ok(Some(v))
            };
            let description_index = take(TFHD_DESCRIPTION_INDEX)?.unwrap_or(0);
            let trex = self.tracks[track].trex;
            let tfhd_defaults = Defaults {
                duration: take(TFHD_DURATION)?.unwrap_or(trex.duration),
                size: take(TFHD_SIZE)?.unwrap_or(trex.size),
                flags: take(TFHD_FLAGS)?.unwrap_or(trex.flags),
            };
            let tfdt = need(&boxes, b"tfdt")?;
            let tfdt = match moof[tfdt.body] {
                1 => be64(moof, tfdt.body + 4)?,
                _ => u64::from(be32(moof, tfdt.body + 4)?),
            };
            let mut truns = boxes.iter().filter(|c| &c.kind == b"trun");
            let trun = match (truns.next(), truns.next()) {
                (Some(t), None) => t,
                _ => return Err("not a rec clip: it needs one `trun` per `traf`".into()),
            };
            let trun_version = moof[trun.body];
            let trun_flags = be32(moof, trun.body)? & 0x00FF_FFFF;
            if trun_flags & TRUN_DATA_OFFSET == 0 {
                return Err("not a rec clip: a `trun` has no data offset".into());
            }
            let count = be32(moof, trun.body + 4)? as usize;
            let offset = be32(moof, trun.body + 8)? as i32;
            let mut p = trun.body + 12;
            let first_flags = if trun_flags & TRUN_FIRST_FLAGS != 0 {
                p += 4;
                Some(be32(moof, p - 4)?)
            } else {
                None
            };
            let per_sample = [TRUN_DURATION, TRUN_SIZE, TRUN_FLAGS, TRUN_CTO]
                .iter()
                .filter(|&&f| trun_flags & f != 0)
                .count();
            let fits = match per_sample {
                0 => count <= 1 << 24,
                n => count
                    .checked_mul(n * 4)
                    .is_some_and(|len| len <= trun.end - p),
            };
            if !fits {
                return Err(short());
            }
            let mut samples = Vec::with_capacity(count);
            for i in 0..count {
                let mut next = |flag: u32| -> Result<Option<u32>, String> {
                    if trun_flags & flag == 0 {
                        return Ok(None);
                    }
                    let v = be32(moof, p)?;
                    p += 4;
                    Ok(Some(v))
                };
                let duration = next(TRUN_DURATION)?.unwrap_or(tfhd_defaults.duration);
                let size = next(TRUN_SIZE)?.unwrap_or(tfhd_defaults.size);
                let flags = next(TRUN_FLAGS)?
                    .or(if i == 0 { first_flags } else { None })
                    .unwrap_or(tfhd_defaults.flags);
                let cto = next(TRUN_CTO)?.unwrap_or(0);
                samples.push(Sample {
                    duration,
                    size,
                    flags,
                    cto,
                });
            }
            let data = at.checked_add_signed(i64::from(offset)).ok_or_else(short)?;
            let bytes: u64 = samples.iter().map(|s| u64::from(s.size)).sum();
            if data < mdat.0 || data + bytes > mdat.1 {
                return Err("not a rec clip: sample data lies outside its `mdat`".into());
            }
            trafs.push(Traf {
                track,
                tfhd_flags: flags,
                tfhd: tfhd_defaults,
                description_index,
                tfdt,
                trun_version,
                trun_flags,
                samples,
                data,
            });
        }
        let mut video = trafs.iter().filter(|t| t.track == self.video);
        match (video.next(), video.next()) {
            (Some(v), None) if v.samples.first().is_some_and(|s| s.flags & NON_SYNC == 0) => {}
            _ => {
                return Err(
                    "not a rec clip: a fragment doesn't start with a video keyframe".into(),
                );
            }
        }
        Ok(Fragment { trafs })
    }
}

// ---- cutting ------------------------------------------------------------------

/// Where to cut: the kept fragments and, in each, the kept samples per traf.
#[derive(Debug)]
struct Cut {
    first: usize,
    /// Per kept fragment, per traf: the kept sample range.
    keep: Vec<Vec<(usize, usize)>>,
    /// The new time zero and the end, in video ticks of the source.
    t0: u64,
    end: u64,
    video_timescale: u32,
}

impl Cut {
    fn start_secs(&self) -> f64 {
        self.t0 as f64 / f64::from(self.video_timescale)
    }

    fn end_secs(&self) -> f64 {
        self.end as f64 / f64::from(self.video_timescale)
    }
}

impl Layout {
    fn cut(&self, start: f64, end: f64) -> Result<Cut, String> {
        if !start.is_finite() || !end.is_finite() {
            return Err("the trim times must be numbers".into());
        }
        if end <= start {
            return Err("the end must be after the start".into());
        }
        let vt = self.tracks[self.video].timescale;
        let vts = f64::from(vt);
        let clip_end = self.fragments.last().expect("not empty").video(self).end();
        let start = start.max(0.0);
        if start * vts >= clip_end as f64 {
            return Err(format!(
                "the start is past the end of the clip ({:.3} s)",
                clip_end as f64 / vts
            ));
        }

        let first = (self.fragments.iter())
            .rposition(|f| f.video(self).tfdt as f64 <= (start + START_SLACK) * vts)
            .unwrap_or(0);
        let t0 = self.fragments[first].video(self).tfdt;

        // Video: every sample that starts before `end`, and at least one.
        let mut last = first;
        let mut kept_in_last = 0;
        let mut cut_end = t0;
        'fragments: for (i, f) in self.fragments.iter().enumerate().skip(first) {
            let v = f.video(self);
            for (k, t) in v.times().enumerate() {
                if (t as f64) >= end * vts && cut_end > t0 {
                    break 'fragments;
                }
                last = i;
                kept_in_last = k + 1;
                cut_end = t + u64::from(v.samples[k].duration);
            }
        }

        let length = (cut_end - t0) as f64 / vts;
        if length < MIN_DURATION - 1e-9 {
            return Err(format!(
                "the trim would be {length:.2} s long (the start snaps back to the keyframe at \
                 {:.3} s); the shortest is {MIN_DURATION} s",
                t0 as f64 / vts
            ));
        }

        // Other tracks: samples starting in t0..cut_end, compared exactly.
        let mut keep = Vec::new();
        for f in &self.fragments[first..=last] {
            let ranges = (f.trafs.iter())
                .map(|t| {
                    if t.track == self.video {
                        let n = if std::ptr::eq(f, &self.fragments[last]) {
                            kept_in_last
                        } else {
                            t.samples.len()
                        };
                        return (0, n);
                    }
                    let ts = u128::from(self.tracks[t.track].timescale);
                    let at = |tick: u64| u128::from(tick) * u128::from(vt);
                    let times: Vec<u64> = t.times().collect();
                    let lo = times.partition_point(|&a| at(a) < u128::from(t0) * ts);
                    let hi = times.partition_point(|&a| at(a) < u128::from(cut_end) * ts);
                    (lo, hi.max(lo))
                })
                .collect();
            keep.push(ranges);
        }
        Ok(Cut {
            first,
            keep,
            t0,
            end: cut_end,
            video_timescale: vt,
        })
    }
}

// ---- writing ------------------------------------------------------------------

/// One traf of the output.
struct OutTraf<'a> {
    track_id: u32,
    traf: &'a Traf,
    lo: usize,
    hi: usize,
    tfdt: u64,
}

impl OutTraf<'_> {
    fn samples(&self) -> &[Sample] {
        &self.traf.samples[self.lo..self.hi]
    }

    fn bytes(&self) -> u64 {
        self.samples().iter().map(|s| u64::from(s.size)).sum()
    }

    fn end(&self) -> u64 {
        self.tfdt
            + self
                .samples()
                .iter()
                .map(|s| u64::from(s.duration))
                .sum::<u64>()
    }
}

fn put32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_be_bytes());
}

/// Writes a box: 32-bit size, fourcc, then whatever `body` appends.
fn boxed(b: &mut Vec<u8>, kind: &[u8; 4], body: impl FnOnce(&mut Vec<u8>)) {
    let start = b.len();
    put32(b, 0);
    b.extend_from_slice(kind);
    body(b);
    let size = (b.len() - start) as u32;
    b[start..start + 4].copy_from_slice(&size.to_be_bytes());
}

fn full_box(
    b: &mut Vec<u8>,
    kind: &[u8; 4],
    version: u8,
    flags: u32,
    body: impl FnOnce(&mut Vec<u8>),
) {
    boxed(b, kind, |b| {
        put32(b, u32::from(version) << 24 | flags);
        body(b);
    });
}

fn patch(buf: &mut [u8], field: Field, value: u64) {
    if field.wide {
        buf[field.at..field.at + 8].copy_from_slice(&value.to_be_bytes());
    } else {
        let v = u32::try_from(value).unwrap_or(u32::MAX);
        buf[field.at..field.at + 4].copy_from_slice(&v.to_be_bytes());
    }
}

/// Rescales ticks, rounding to nearest.
fn rescale(ticks: u64, from: u32, to: u32) -> u64 {
    let from = u128::from(from);
    ((u128::from(ticks) * u128::from(to) + from / 2) / from) as u64
}

impl Layout {
    /// The output fragments: kept samples with rebased `tfdt`s, empty trafs
    /// and fragments left out.
    fn output<'a>(&'a self, cut: &Cut) -> Vec<Vec<OutTraf<'a>>> {
        let vt = self.tracks[self.video].timescale;
        let mut out = Vec::new();
        for (f, ranges) in self.fragments[cut.first..].iter().zip(&cut.keep) {
            let trafs: Vec<OutTraf<'a>> = (f.trafs.iter().zip(ranges))
                .filter(|(_, (lo, hi))| hi > lo)
                .map(|(traf, &(lo, hi))| {
                    let ts = self.tracks[traf.track].timescale;
                    let first: u64 = traf.times().nth(lo).expect("in range");
                    // Never negative: kept samples start at or after t0.
                    let tfdt = first.saturating_sub(rescale(cut.t0, vt, ts));
                    OutTraf {
                        track_id: self.tracks[traf.track].id,
                        traf,
                        lo,
                        hi,
                        tfdt,
                    }
                })
                .collect();
            if !trafs.is_empty() {
                out.push(trafs);
            }
        }
        out
    }

    fn write(&self, cut: &Cut, src: &mut File, out: &mut impl Write) -> io::Result<()> {
        let fragments = self.output(cut);

        // Durations: each track ends with its last kept sample.
        let mut moov = self.moov.clone();
        let mut movie = 0;
        for (i, track) in self.tracks.iter().enumerate() {
            let media = (fragments.iter().flatten())
                .filter(|t| t.traf.track == i)
                .map(OutTraf::end)
                .max()
                .unwrap_or(0);
            let units = rescale(media, track.timescale, self.movie_timescale);
            movie = movie.max(units);
            patch(&mut moov, track.mdhd, media);
            patch(&mut moov, track.tkhd, units);
        }
        patch(&mut moov, self.mvhd, movie);
        if let Some(mehd) = self.mehd {
            patch(&mut moov, mehd, movie);
        }
        out.write_all(&self.ftyp)?;
        out.write_all(&moov)?;

        for (n, trafs) in fragments.iter().enumerate() {
            let payload: u64 = trafs.iter().map(OutTraf::bytes).sum();
            let wide_mdat = payload + 8 > u64::from(u32::MAX);
            let (moof, slots) = build_moof(n as u32 + 1, trafs);
            let mut moof = moof;
            let mut offset = moof.len() as u64 + if wide_mdat { 16 } else { 8 };
            for (slot, t) in slots.into_iter().zip(trafs) {
                let v = i32::try_from(offset)
                    .map_err(|_| io::Error::other("a fragment is too large"))?;
                moof[slot..slot + 4].copy_from_slice(&v.to_be_bytes());
                offset += t.bytes();
            }
            out.write_all(&moof)?;
            if wide_mdat {
                out.write_all(&1u32.to_be_bytes())?;
                out.write_all(b"mdat")?;
                out.write_all(&(payload + 16).to_be_bytes())?;
            } else {
                out.write_all(&((payload + 8) as u32).to_be_bytes())?;
                out.write_all(b"mdat")?;
            }
            for t in trafs {
                src.seek(SeekFrom::Start(t.traf.data + t.traf.bytes_before(t.lo)))?;
                let bytes = t.bytes();
                if io::copy(&mut Read::take(&mut *src, bytes), out)? != bytes {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
            }
        }
        out.flush()
    }
}

/// Builds a `moof` for `trafs`, keeping each source traf's flags and
/// defaults. Returns it with the positions of the `trun` data offsets,
/// which the caller fills in.
fn build_moof(sequence: u32, trafs: &[OutTraf<'_>]) -> (Vec<u8>, Vec<usize>) {
    let mut b =
        Vec::with_capacity(256 + trafs.iter().map(|t| t.samples().len() * 16).sum::<usize>());
    let mut slots = Vec::with_capacity(trafs.len());
    boxed(&mut b, b"moof", |b| {
        full_box(b, b"mfhd", 0, 0, |b| put32(b, sequence));
        for t in trafs {
            let src = t.traf;
            boxed(b, b"traf", |b| {
                let kept = TFHD_DESCRIPTION_INDEX | TFHD_DURATION | TFHD_SIZE | TFHD_FLAGS;
                let flags = src.tfhd_flags & kept | TFHD_BASE_IS_MOOF;
                full_box(b, b"tfhd", 0, flags, |b| {
                    put32(b, t.track_id);
                    if flags & TFHD_DESCRIPTION_INDEX != 0 {
                        put32(b, src.description_index);
                    }
                    if flags & TFHD_DURATION != 0 {
                        put32(b, src.tfhd.duration);
                    }
                    if flags & TFHD_SIZE != 0 {
                        put32(b, src.tfhd.size);
                    }
                    if flags & TFHD_FLAGS != 0 {
                        put32(b, src.tfhd.flags);
                    }
                });
                full_box(b, b"tfdt", 1, 0, |b| {
                    b.extend_from_slice(&t.tfdt.to_be_bytes())
                });
                // Durations and sizes are always written: the last sample of
                // a shortened run may differ from any default.
                let flags = src.trun_flags | TRUN_DATA_OFFSET | TRUN_DURATION | TRUN_SIZE;
                full_box(b, b"trun", src.trun_version, flags, |b| {
                    let samples = t.samples();
                    put32(b, samples.len() as u32);
                    slots.push(b.len());
                    put32(b, 0); // data offset, patched by the caller
                    if flags & TRUN_FIRST_FLAGS != 0 {
                        put32(b, samples[0].flags);
                    }
                    for s in samples {
                        put32(b, s.duration);
                        put32(b, s.size);
                        if flags & TRUN_FLAGS != 0 {
                            put32(b, s.flags);
                        }
                        if flags & TRUN_CTO != 0 {
                            put32(b, s.cto);
                        }
                    }
                });
            });
        }
    });
    (b, slots)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::process::Command;
    use std::sync::Arc;

    use rec_core::media::{
        AudioCodec, AudioFormat, TrackInfo, TrackKind, VIDEO_TRACK, VideoCodec, VideoColor,
        VideoFormat,
    };
    use rec_core::replay::Packet;
    use rec_core::time::{HNS_PER_SEC, Hns};
    use serde_json::json;

    use super::*;
    use crate::clip::{Event, Game};

    // rec-mux's fixtures: 3 s of 30 fps video, an IDR every 30 frames, and
    // 3.2 s of 48 kHz stereo AAC.
    const H264: &[u8] = include_bytes!("../../rec-mux/tests/fixtures/h264_320x240_30fps.h264");
    const HEVC: &[u8] = include_bytes!("../../rec-mux/tests/fixtures/hevc_320x240_30fps.h265");
    const AAC: &[u8] = include_bytes!("../../rec-mux/tests/fixtures/aac_48k_stereo.aac");

    /// A QPC-like time zero.
    const BASE: Hns = 123_456_789_000_000;

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("rec-library-trim-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Annex-B access units (one slice per picture) and whether each is IDR.
    fn access_units(codec: VideoCodec, stream: &[u8]) -> Vec<(Vec<u8>, bool)> {
        let mut starts = Vec::new();
        let mut i = 0;
        while i + 3 <= stream.len() {
            if stream[i..i + 3] == [0, 0, 1] {
                starts.push(i + 3);
                i += 3;
            } else {
                i += 1;
            }
        }
        let mut aus = Vec::new();
        let mut cur = Vec::new();
        for (k, &s) in starts.iter().enumerate() {
            let mut e = starts.get(k + 1).map_or(stream.len(), |n| n - 3);
            while e > s && stream[e - 1] == 0 {
                e -= 1;
            }
            let nal = &stream[s..e];
            cur.extend_from_slice(&[0, 0, 0, 1]);
            cur.extend_from_slice(nal);
            let (vcl, idr) = match codec {
                VideoCodec::H264 => {
                    let t = nal[0] & 0x1F;
                    ((1..=5).contains(&t), t == 5)
                }
                _ => {
                    let t = (nal[0] >> 1) & 0x3F;
                    (t < 32, (19..=20).contains(&t))
                }
            };
            if vcl {
                aus.push((std::mem::take(&mut cur), idr));
            }
        }
        aus
    }

    /// Raw AAC frames and the format of an ADTS stream (48 kHz stereo).
    fn adts(stream: &[u8]) -> (Vec<Vec<u8>>, AudioFormat) {
        let mut frames = Vec::new();
        let mut pos = 0;
        let mut asc = 0u16;
        while pos + 7 <= stream.len() {
            let header = if stream[pos + 1] & 1 == 1 { 7 } else { 9 };
            let len = (usize::from(stream[pos + 3] & 3) << 11)
                | (usize::from(stream[pos + 4]) << 3)
                | usize::from(stream[pos + 5] >> 5);
            let profile = u16::from(stream[pos + 2] >> 6);
            let sfi = u16::from((stream[pos + 2] >> 2) & 0xF);
            let channels = u16::from(((stream[pos + 2] & 1) << 2) | (stream[pos + 3] >> 6));
            asc = ((profile + 1) << 11) | (sfi << 7) | (channels << 3);
            frames.push(stream[pos + header..pos + len].to_vec());
            pos += len;
        }
        let format = AudioFormat {
            codec: AudioCodec::Aac,
            sample_rate: 48_000,
            channels: 2,
            codec_config: asc.to_be_bytes().to_vec(),
        };
        (frames, format)
    }

    /// A clip as recd saves it, the audio starting 30 ms before the video.
    fn make_clip(codec: VideoCodec, stream: &[u8], audio: bool) -> Vec<u8> {
        // (arrival time, packet): video after its encode latency, audio once
        // its last sample is captured.
        let mut packets: Vec<(Hns, Packet)> = access_units(codec, stream)
            .into_iter()
            .enumerate()
            .map(|(i, (data, keyframe))| {
                let pts = BASE + (i as i64 * HNS_PER_SEC + 15) / 30;
                let p = Packet {
                    track: VIDEO_TRACK,
                    segment: 0,
                    pts,
                    dts: pts,
                    duration: HNS_PER_SEC / 30,
                    keyframe,
                    data: Arc::from(data.as_slice()),
                };
                (pts + 1_000_000, p)
            })
            .collect();
        let mut tracks = vec![TrackInfo {
            id: VIDEO_TRACK,
            name: "Video".into(),
            kind: TrackKind::Video(VideoFormat {
                codec,
                width: 320,
                height: 240,
                fps: 30,
                color: VideoColor::Sdr,
            }),
        }];
        if audio {
            let (frames, format) = adts(AAC);
            let start = BASE - 300_000;
            for (i, f) in frames.iter().enumerate() {
                let pts = start + (i as i64 * 1024 * HNS_PER_SEC + 24_000) / 48_000;
                let duration = 1024 * HNS_PER_SEC / 48_000;
                let p = Packet {
                    track: 1,
                    segment: 0,
                    pts,
                    dts: pts,
                    duration,
                    keyframe: true,
                    data: Arc::from(f.as_slice()),
                };
                packets.push((pts + duration, p));
            }
            tracks.push(TrackInfo {
                id: 1,
                name: "Game".into(),
                kind: TrackKind::Audio(format),
            });
        }
        packets.sort_by_key(|(arrival, _)| *arrival);
        let packets: Vec<Packet> = packets.into_iter().map(|(_, p)| p).collect();
        rec_mux::write_clip(Cursor::new(Vec::new()), tracks, &packets)
            .unwrap()
            .into_inner()
    }

    fn read(path: &Path) -> Layout {
        Layout::read(&mut File::open(path).unwrap()).unwrap()
    }

    /// Every sample of track index `track`: start time and bytes.
    fn samples(path: &Path, track: usize) -> Vec<(u64, Vec<u8>)> {
        let layout = read(path);
        let mut file = File::open(path).unwrap();
        let mut out = Vec::new();
        for traf in layout.fragments.iter().flat_map(|f| &f.trafs) {
            if traf.track != track {
                continue;
            }
            let mut at = traf.data;
            for (t, s) in traf.times().zip(&traf.samples) {
                let mut data = vec![0; s.size as usize];
                file.seek(SeekFrom::Start(at)).unwrap();
                file.read_exact(&mut data).unwrap();
                out.push((t, data));
                at += u64::from(s.size);
            }
        }
        out
    }

    fn field(layout: &Layout, f: Field) -> u64 {
        let b = &layout.moov[f.at..];
        if f.wide {
            u64::from_be_bytes(b[..8].try_into().unwrap())
        } else {
            u64::from(u32::from_be_bytes(b[..4].try_into().unwrap()))
        }
    }

    /// The source sidecar: times chosen around a 1.0..2.533 s cut.
    fn sidecar(file: &str) -> Value {
        json!({
            "rec": 1,
            "clip": {
                "id": "01SOURCECLIP00000000000000",
                "file": file,
                "parent": null,
                "duration": 3.0,
                "trigger": "hotkey",
                "saved_at": 2.2
            },
            "game": { "name": "Game", "exe": "game.exe", "steam_appid": null },
            "time": {
                "start_utc": "2026-10-18T01:04:03.112Z",
                "utc_offset_min": -240,
                "session": "01SESSION",
                "session_offset": 100.0
            },
            "video": { "codec": "h264", "width": 320, "height": 240, "fps": 30, "hdr": false,
                       "dropped_frames": 3 },
            "audio": [{ "index": 0, "name": "Game", "role": "game",
                        "loudness": (0..30).collect::<Vec<u32>>() }],
            "events": [
                { "t": 0.5, "kind": "marker", "src": "hotkey", "conf": 1 },
                { "t": 0.8, "kind": "loud", "src": "audio:game", "conf": 0.7, "dur": 0.5 },
                { "t": 1.2, "kind": "marker", "src": "cli", "conf": 1, "extra": "kept" },
                { "t": 2.4, "kind": "loud", "src": "audio:game", "conf": 0.7, "dur": 1.0 },
                { "t": 2.9, "kind": "marker", "src": "hotkey", "conf": 1 }
            ],
            "loudness": { "hz": 10, "scale": "RMS level" },
            "analysis": { "thumbnail": 1.0 },
            "starred": true,
            "future_field": { "kept": true }
        })
    }

    /// A saved clip with its sidecar, in `dir`.
    fn saved(dir: &Path, name: &str, bytes: &[u8]) -> Clip {
        let video = dir.join(format!("{name}.mp4"));
        fs::write(&video, bytes).unwrap();
        let path = sidecar_for(&video);
        write_sidecar(&path, &sidecar(&format!("{name}.mp4"))).unwrap();
        Clip::load(&path).unwrap()
    }

    #[test]
    fn cuts_at_the_keyframe_and_the_frame_after_the_end() {
        let dir = Dir::new("cut");
        let clip = saved(&dir.0, "Game", &make_clip(VideoCodec::H264, H264, true));
        let source = clip.video.clone().unwrap();
        let source_bytes = fs::read(&source).unwrap();
        let (start, end) = plan(&source, 1.4, 2.51).unwrap();
        assert_eq!(start, 1.0);
        // Frame 75 starts at 2.5 s, before the end, so the cut ends after it.
        assert!((end - 76.0 / 30.0).abs() < 1e-4, "{end}");

        let t = trim(&clip, 1.4, 2.51, None).unwrap();
        assert_eq!((t.start, t.end), (start, end));
        assert_eq!(t.video, dir.0.join("Game trim.mp4"));

        let out = read(&t.video);
        let video = out.video;
        let audio = 1 - video;
        let tfdts: Vec<u64> = out.fragments.iter().map(|f| f.video(&out).tfdt).collect();
        assert_eq!(tfdts, [0, 90_000]);
        let counts: Vec<usize> = (out.fragments.iter())
            .map(|f| f.video(&out).samples.len())
            .collect();
        assert_eq!(counts, [30, 16]);

        // Video: frames 30..76, byte for byte, rebased to 0.
        let src_video = samples(&source, video);
        let new_video = samples(&t.video, video);
        assert_eq!(new_video.len(), 46);
        for (new, old) in new_video.iter().zip(&src_video[30..76]) {
            assert_eq!(new.0, old.0 - 90_000);
            assert_eq!(new.1, old.1);
        }

        // Audio: frames starting in 1.0 s..end, keeping their offset.
        let end_ticks = 228_000u64;
        let expected: Vec<(u64, Vec<u8>)> = samples(&source, audio)
            .into_iter()
            .filter(|(t, _)| *t >= 48_000 && t * 90_000 < end_ticks * 48_000)
            .collect();
        let new_audio = samples(&t.video, audio);
        assert!(expected.len() > 60);
        assert_eq!(new_audio.len(), expected.len());
        for (new, old) in new_audio.iter().zip(&expected) {
            assert_eq!(new.0, old.0 - 48_000);
            assert_eq!(new.1, old.1);
        }

        // Durations.
        let tracks = &out.tracks;
        assert_eq!(field(&out, tracks[video].mdhd), end_ticks - 90_000);
        assert_eq!(field(&out, tracks[video].tkhd), 1533);
        let audio_end = new_audio.last().unwrap().0 + 1024;
        assert_eq!(field(&out, tracks[audio].mdhd), audio_end);
        let movie = field(&out, out.mvhd);
        assert_eq!(movie, 1533.max((audio_end * 1000 + 24_000) / 48_000));
        assert_eq!(field(&out, out.mehd.unwrap()), movie);

        // Sequence numbers start again at 1.
        let bytes = fs::read(&t.video).unwrap();
        let mfhd: Vec<u32> = (bytes.windows(4).enumerate())
            .filter(|(_, w)| w == b"mfhd")
            .map(|(i, _)| u32::from_be_bytes(bytes[i + 8..i + 12].try_into().unwrap()))
            .collect();
        assert_eq!(mfhd, [1, 2]);
        // The source is untouched, and no temp file is left.
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert!(!dir.0.join("Game trim.mp4.tmp").exists());
    }

    #[test]
    fn the_whole_clip_comes_through_unchanged() {
        let dir = Dir::new("whole");
        let clip = saved(&dir.0, "Game", &make_clip(VideoCodec::H264, H264, true));
        let source = clip.video.clone().unwrap();
        let t = trim(&clip, 0.0, 100.0, None).unwrap();
        assert_eq!((t.start, t.end), (0.0, 3.0));
        let (a, b) = (read(&source), read(&t.video));
        for track in 0..2 {
            assert_eq!(samples(&source, track), samples(&t.video, track));
            let (ta, tb) = (&a.tracks[track], &b.tracks[track]);
            assert_eq!(field(&a, ta.mdhd), field(&b, tb.mdhd));
            assert_eq!(field(&a, ta.tkhd), field(&b, tb.tkhd));
        }
        assert_eq!(field(&a, a.mvhd), field(&b, b.mvhd));
        // rec-mux and the trim lay the boxes out alike.
        assert_eq!(fs::read(&source).unwrap(), fs::read(&t.video).unwrap());
    }

    #[test]
    fn hevc_is_cut_the_same_way() {
        let dir = Dir::new("hevc");
        let path = dir.0.join("hevc.mp4");
        fs::write(&path, make_clip(VideoCodec::Hevc, HEVC, false)).unwrap();
        let mut clip = Clip::from_value(sidecar("hevc.mp4")).unwrap();
        clip.video = Some(path.clone());
        let out = dir.0.join("out.mp4");
        let t = trim(&clip, 0.5, 1.7, Some(&out)).unwrap();
        assert_eq!(t.video, out);
        assert_eq!(t.sidecar, Some(dir.0.join("out.rec.json")));
        assert_eq!(t.start, 0.0);
        // Frame 51 starts at 1.7 s exactly: it is not kept.
        assert!((t.end - 1.7).abs() < 1e-9, "{}", t.end);
        assert_eq!(samples(&out, 0), samples(&path, 0)[..51]);
        assert_eq!(read(&out).fragments.len(), 2);
    }

    #[test]
    fn carries_the_sidecar_over() {
        let dir = Dir::new("sidecar");
        let clip = saved(&dir.0, "Game", &make_clip(VideoCodec::H264, H264, true));
        let t = trim(&clip, 1.4, 2.51, None).unwrap();
        let new = Clip::load(t.sidecar.as_deref().unwrap()).unwrap();
        assert_eq!(new.parent.as_deref(), Some("01SOURCECLIP00000000000000"));
        assert_eq!(new.id.len(), 26);
        assert_ne!(new.id, clip.id);
        assert_eq!(new.file, "Game trim.mp4");
        assert_eq!(new.video.as_deref(), Some(t.video.as_path()));
        assert_eq!(new.duration, 1.533);
        assert_eq!(new.saved_at, Some(1.2));
        assert_eq!(new.raw["time"]["start_utc"], "2026-10-18T01:04:04.112Z");
        assert_eq!(new.session_offset, Some(101.0));
        assert_eq!(new.utc_offset_min, Some(-240));
        let events: Vec<(f64, Option<f64>, &str)> = (new.events.iter())
            .map(|e| (e.t, e.dur, e.src.as_str()))
            .collect();
        assert_eq!(
            events,
            [
                (0.0, Some(0.3), "audio:game"),
                (0.2, None, "cli"),
                (1.4, Some(0.133), "audio:game"),
            ]
        );
        assert_eq!(new.events[1].raw["extra"], "kept");
        assert_eq!(new.audio[0].loudness, (10..25).collect::<Vec<u8>>());
        assert!(new.analysis.is_none());
        assert!(!new.starred);
        assert_eq!(new.raw["future_field"]["kept"], true);
        assert!(new.raw["video"].get("dropped_frames").is_none());
        assert_eq!(new.raw["video"]["codec"], "h264");

        // Outside the cut, `saved_at` goes; a second trim gets a new name.
        let t2 = trim(&clip, 0.0, 1.0, None).unwrap();
        assert_eq!(t2.video, dir.0.join("Game trim 2.mp4"));
        let new = Clip::load(t2.sidecar.as_deref().unwrap()).unwrap();
        assert_eq!(new.saved_at, None);
        assert!(new.raw["clip"]["saved_at"].is_null());
    }

    #[test]
    fn a_journal_only_clip_gets_a_sidecar() {
        let dir = Dir::new("journal");
        let video = dir.0.join("j.mp4");
        fs::write(&video, make_clip(VideoCodec::H264, H264, true)).unwrap();
        let marker = json!({"t": 1.5, "kind": "marker", "src": "hotkey", "conf": 1});
        let clip = Clip::from_journal(
            "01JOURNALCLIP",
            &video,
            "01SESSION",
            10.0,
            13.0,
            None,
            Game::default(),
            Some(1000.0),
            Some(0),
            vec![Event::from_value(&marker).unwrap()],
        );
        let t = trim(&clip, 1.0, 3.0, None).unwrap();
        let new = Clip::load(t.sidecar.as_deref().unwrap()).unwrap();
        assert!(new.raw.get("source").is_none());
        assert_eq!(new.parent.as_deref(), Some("01JOURNALCLIP"));
        assert_eq!(new.session_offset, Some(11.0));
        assert_eq!(new.start_utc, Some(1011.0));
        assert_eq!(new.events.len(), 1);
        assert_eq!(new.events[0].t, 0.5);
    }

    #[test]
    fn refuses_bad_ranges_and_other_files() {
        let dir = Dir::new("refuse");
        let clip = saved(&dir.0, "Game", &make_clip(VideoCodec::H264, H264, false));
        let path = clip.video.clone().unwrap();
        assert!(
            plan(&path, 2.0, 1.0)
                .unwrap_err()
                .contains("after the start")
        );
        assert!(plan(&path, 1.0, 1.0).is_err());
        // 0.1..0.3 snaps to 0..0.3: too short.
        let short = plan(&path, 0.1, 0.3).unwrap_err();
        assert!(short.contains("shortest"), "{short}");
        assert!(plan(&path, 3.5, 4.0).unwrap_err().contains("past the end"));
        assert!(plan(&path, f64::NAN, 1.0).is_err());
        // A start just under a keyframe, as a rounded display shows it.
        assert_eq!(plan(&path, 1.9999, 2.6).unwrap().0, 2.0);
        assert_eq!(plan(&path, -3.0, 1.0).unwrap().0, 0.0);

        let err = trim(&clip, 0.0, 1.0, Some(&path)).unwrap_err();
        assert!(err.contains("replace"), "{err}");

        let other = dir.0.join("other.mp4");
        fs::write(&other, b"not a video at all").unwrap();
        assert!(
            plan(&other, 0.0, 1.0)
                .unwrap_err()
                .contains("not a rec clip")
        );
        // An MP4 without fragments.
        let mut plain = Vec::new();
        boxed(&mut plain, b"ftyp", |b| {
            b.extend_from_slice(b"isom\0\0\0\0isom")
        });
        boxed(&mut plain, b"moov", |b| {
            full_box(b, b"mvhd", 0, 0, |b| {
                b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 232]);
                b.extend_from_slice(&[0; 84]);
            });
        });
        fs::write(&other, &plain).unwrap();
        let err = plan(&other, 0.0, 1.0).unwrap_err();
        assert!(err.contains("not a fragmented MP4"), "{err}");
    }

    #[test]
    fn a_cut_off_clip_trims_up_to_its_last_whole_fragment() {
        let dir = Dir::new("cutoff");
        let mut bytes = make_clip(VideoCodec::H264, H264, true);
        bytes.truncate(bytes.len() - 100);
        let path = dir.0.join("crashed.mp4");
        fs::write(&path, &bytes).unwrap();
        assert_eq!(plan(&path, 0.5, 100.0).unwrap(), (0.0, 2.0));
    }

    /// ffprobe decodes the trim without complaint. Needs ffprobe on PATH:
    /// `cargo test -p rec-library ffprobe -- --ignored`.
    #[test]
    #[ignore]
    fn ffprobe_reads_the_trim() {
        let dir = Dir::new("ffprobe");
        let clip = saved(&dir.0, "Game", &make_clip(VideoCodec::H264, H264, true));
        let t = trim(&clip, 1.4, 2.51, None).unwrap();
        let probe = Command::new("ffprobe")
            .args(["-v", "error", "-count_frames", "-show_entries"])
            .args(["stream=codec_type,nb_read_frames:format=duration"])
            .args(["-of", "compact"])
            .arg(&t.video)
            .output()
            .expect("ffprobe on PATH");
        let stdout = String::from_utf8_lossy(&probe.stdout);
        let stderr = String::from_utf8_lossy(&probe.stderr);
        assert!(
            probe.status.success() && stderr.trim().is_empty(),
            "{stderr}"
        );
        assert!(
            stdout.contains("codec_type=video|nb_read_frames=46"),
            "{stdout}"
        );
        assert!(stdout.contains("codec_type=audio"), "{stdout}");
        assert!(stdout.contains("duration=1.5"), "{stdout}");
    }
}
