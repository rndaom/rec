//! The streaming fragmented MP4 writer.

use std::collections::VecDeque;
use std::fmt;
use std::io::{Seek, SeekFrom, Write};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rec_core::media::{AudioCodec, TrackInfo, TrackKind as InfoKind, VIDEO_TRACK, VideoFormat};
use rec_core::replay::{Packet, TrackId};
use rec_core::time::{HNS_PER_SEC, Hns};

use crate::error::MuxError;
use crate::fragment::{TrafSpec, TrunSample, build_moof};
use crate::init::{
    FLAGS_NON_SYNC, FLAGS_SYNC, InitTrack, MOVIE_TIMESCALE, Patches, TrackKind, build_init,
};
use crate::video::{CodecConfig, build_config, convert_access_unit};

/// Video media timescale. 90 kHz divides the common frame rates (24, 25, 30,
/// 50, 60, 120, 144 fps) exactly, and a tick is 111 ns, far finer than the
/// capture clock jitter.
pub const VIDEO_TIMESCALE: u32 = 90_000;

/// Samples per AAC-LC access unit.
const AAC_FRAME: i64 = 1024;

/// Audio that arrives before the first video keyframe is held back until the
/// keyframe's timestamp (the clip start) is known. This bounds the memory a
/// stream with no video can use: about five seconds of AAC frames.
const MAX_AUDIO_BEFORE_VIDEO: usize = 256;

/// Seconds between the MP4 epoch (1904-01-01) and the Unix epoch.
const MP4_EPOCH_OFFSET: u64 = 2_082_844_800;

/// Converts a time relative to the clip start to `timescale` ticks, rounding
/// to nearest. Absolute timestamps are converted (never durations), so
/// rounding errors cannot accumulate.
fn to_ticks(rel: Hns, timescale: u32) -> i64 {
    let num = i128::from(rel) * i128::from(timescale) + i128::from(HNS_PER_SEC / 2);
    num.div_euclid(i128::from(HNS_PER_SEC)) as i64
}

/// Rescales a non-negative tick count to the movie timescale, rounding to
/// nearest.
fn to_movie_units(ticks: u64, timescale: u32) -> u64 {
    let ts = u64::from(timescale);
    (ticks * u64::from(MOVIE_TIMESCALE) + ts / 2) / ts
}

fn clamp_u32(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[derive(Debug)]
struct VideoSample {
    /// Decode (= presentation) time in video ticks since the clip start.
    ts: i64,
    size: u32,
    sync: bool,
}

#[derive(Debug)]
struct VideoState {
    format: VideoFormat,
    name: String,
    /// Length-prefixed sample data of the open fragment, back to back.
    data: Vec<u8>,
    samples: Vec<VideoSample>,
    /// Clip-relative pts and duration of the newest sample, in Hns.
    last_rel: Hns,
    last_ts: i64,
    last_duration: Hns,
    prev_rel: Option<Hns>,
    /// End of the last flushed sample, in video ticks.
    media_end: u64,
}

#[derive(Debug)]
struct AudioSample {
    /// Decode time in track ticks since the clip start.
    dts: i64,
    data: Arc<[u8]>,
}

#[derive(Debug)]
struct AudioState {
    id: TrackId,
    name: String,
    sample_rate: u32,
    channels: u16,
    config: Vec<u8>,
    enabled: bool,
    /// Packets that arrived before the first video keyframe.
    before_video: VecDeque<Packet>,
    /// Where the next frame goes if it is contiguous; `None` before the first.
    next_dts: Option<i64>,
    pending: Vec<AudioSample>,
    /// End of the last flushed sample, in track ticks.
    media_end: u64,
}

#[derive(Clone, Copy, Debug)]
enum End {
    /// A new keyframe at this clip-relative time starts the next fragment.
    Keyframe(Hns),
    /// The clip ends.
    Final,
}

enum Target {
    Video,
    Audio(usize),
}

/// Streaming fragmented-MP4 writer. See the [crate documentation](crate).
pub struct Mp4Writer<W: Write + Seek> {
    out: W,
    pos: u64,
    video: VideoState,
    audio: Vec<AudioState>,
    /// Segment of the first video packet; every packet must match it.
    segment: Option<u32>,
    /// Absolute pts of the first video keyframe: the clip's time zero.
    base: Option<Hns>,
    config: Option<CodecConfig>,
    patches: Option<Patches>,
    fragments: u32,
    poisoned: bool,
}

impl<W: Write + Seek> fmt::Debug for Mp4Writer<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mp4Writer")
            .field("bytes_written", &self.pos)
            .field("fragments", &self.fragments)
            .field("audio_tracks", &self.audio.len())
            .field("poisoned", &self.poisoned)
            .finish_non_exhaustive()
    }
}

impl<W: Write + Seek> Mp4Writer<W> {
    /// Creates a writer for `tracks`. Track [`VIDEO_TRACK`] (id 0) must be
    /// present and be a video track; every other track must be AAC audio.
    /// Nothing is written until the first fragment is complete.
    ///
    /// The first audio track is enabled; further audio tracks are present but
    /// disabled, so players default to the first (editors still list them).
    pub fn new(out: W, tracks: Vec<TrackInfo>) -> Result<Self, MuxError> {
        let mut video = None;
        let mut audio: Vec<AudioState> = Vec::new();
        for t in &tracks {
            if tracks.iter().filter(|o| o.id == t.id).count() > 1 {
                return Err(MuxError::InvalidTracks(format!(
                    "duplicate track id {}",
                    t.id
                )));
            }
            match &t.kind {
                InfoKind::Video(format) => {
                    if t.id != VIDEO_TRACK || video.is_some() {
                        return Err(MuxError::InvalidTracks(
                            "the only video track must have id 0".into(),
                        ));
                    }
                    let (w, h) = (format.width, format.height);
                    if w == 0 || h == 0 || w > u32::from(u16::MAX) || h > u32::from(u16::MAX) {
                        return Err(MuxError::InvalidTracks(format!(
                            "unsupported video size {w}x{h}"
                        )));
                    }
                    video = Some(VideoState {
                        format: format.clone(),
                        name: non_empty(&t.name, "VideoHandler"),
                        data: Vec::new(),
                        samples: Vec::new(),
                        last_rel: 0,
                        last_ts: 0,
                        last_duration: 0,
                        prev_rel: None,
                        media_end: 0,
                    });
                }
                InfoKind::Audio(format) => {
                    if t.id == VIDEO_TRACK {
                        return Err(MuxError::InvalidTracks(
                            "track 0 must be the video track".into(),
                        ));
                    }
                    let AudioCodec::Aac = format.codec;
                    if format.sample_rate == 0 || format.channels == 0 {
                        return Err(MuxError::InvalidTracks(format!(
                            "audio track {} has no sample rate or channels",
                            t.id
                        )));
                    }
                    if format.codec_config.is_empty() {
                        return Err(MuxError::InvalidTracks(format!(
                            "audio track {} has an empty codec_config",
                            t.id
                        )));
                    }
                    audio.push(AudioState {
                        id: t.id,
                        name: non_empty(&t.name, "SoundHandler"),
                        sample_rate: format.sample_rate,
                        channels: format.channels,
                        config: format.codec_config.clone(),
                        enabled: audio.is_empty(),
                        before_video: VecDeque::new(),
                        next_dts: None,
                        pending: Vec::new(),
                        media_end: 0,
                    });
                }
            }
        }
        let video = video.ok_or_else(|| MuxError::InvalidTracks("no video track (id 0)".into()))?;
        Ok(Self {
            out,
            pos: 0,
            video,
            audio,
            segment: None,
            base: None,
            config: None,
            patches: None,
            fragments: 0,
            poisoned: false,
        })
    }

    /// Bytes handed to the output so far.
    pub fn bytes_written(&self) -> u64 {
        self.pos
    }

    /// Number of complete fragments written so far. Everything before the
    /// current open GOP is on disk and survives a crash.
    pub fn fragments_written(&self) -> u32 {
        self.fragments
    }

    /// Adds one packet. Packets arrive in capture order with the tracks
    /// interleaved; audio may precede the video of the same timestamp.
    ///
    /// Video before the first keyframe and audio before the clip start are
    /// dropped. A failed call leaves the writer usable unless the error was an
    /// I/O error, after which every call returns [`MuxError::Poisoned`].
    pub fn write(&mut self, packet: &Packet) -> Result<(), MuxError> {
        if self.poisoned {
            return Err(MuxError::Poisoned);
        }
        let result = self.write_inner(packet);
        if matches!(result, Err(MuxError::Io(_))) {
            self.poisoned = true;
        }
        result
    }

    /// Flushes the last fragment, patches the durations in the header and
    /// returns the output, positioned at the end of the file.
    pub fn finish(mut self) -> Result<W, MuxError> {
        if self.poisoned {
            return Err(MuxError::Poisoned);
        }
        if self.base.is_none() {
            return Err(MuxError::NoVideo);
        }
        self.flush(End::Final)?;
        self.patch_durations()?;
        self.out.flush()?;
        Ok(self.out)
    }

    fn target(&self, track: TrackId) -> Result<Target, MuxError> {
        if track == VIDEO_TRACK {
            Ok(Target::Video)
        } else {
            self.audio
                .iter()
                .position(|a| a.id == track)
                .map(Target::Audio)
                .ok_or(MuxError::UnknownTrack(track))
        }
    }

    fn write_inner(&mut self, packet: &Packet) -> Result<(), MuxError> {
        let target = self.target(packet.track)?;
        if let Some(expected) = self.segment {
            if packet.segment != expected {
                return Err(MuxError::SegmentChange {
                    expected,
                    found: packet.segment,
                });
            }
        }
        match target {
            Target::Video => self.write_video(packet),
            Target::Audio(i) => self.write_audio(i, packet),
        }
    }

    fn write_video(&mut self, packet: &Packet) -> Result<(), MuxError> {
        if self.segment.is_none() {
            // Audio held back before the first video packet must belong to
            // the same encoder configuration.
            for a in &self.audio {
                if let Some(p) = a.before_video.iter().find(|p| p.segment != packet.segment) {
                    return Err(MuxError::SegmentChange {
                        expected: packet.segment,
                        found: p.segment,
                    });
                }
            }
            self.segment = Some(packet.segment);
        }

        let Some(base) = self.base else {
            if !packet.keyframe {
                return Ok(()); // video before the first keyframe is dropped
            }
            return self.start_clip(packet);
        };

        let rel = packet
            .pts
            .checked_sub(base)
            .ok_or_else(|| MuxError::malformed("video packet", "timestamp out of range"))?;
        let ts = to_ticks(rel, VIDEO_TIMESCALE);
        let previous_rel = self.video.last_rel;
        if rel <= previous_rel || ts <= self.video.last_ts {
            return Err(MuxError::NonMonotonicVideo {
                previous: base.saturating_add(previous_rel),
                found: packet.pts,
            });
        }
        let converted = convert_access_unit(self.video.format.codec, &packet.data)?;
        let size = u32::try_from(converted.sample.len())
            .map_err(|_| MuxError::TooLarge("video sample"))?;
        if packet.keyframe {
            self.flush_checked(End::Keyframe(rel))?;
        }
        self.push_video(rel, ts, packet, converted.sample, size);
        Ok(())
    }

    /// Handles the first keyframe: builds the codec configuration, fixes the
    /// clip's time zero, and releases the audio that was waiting for it.
    fn start_clip(&mut self, packet: &Packet) -> Result<(), MuxError> {
        let converted = convert_access_unit(self.video.format.codec, &packet.data)?;
        let config = build_config(self.video.format.codec, &converted.params)?;
        let size = u32::try_from(converted.sample.len())
            .map_err(|_| MuxError::TooLarge("video sample"))?;
        self.config = Some(config);
        self.write_init()?;
        self.base = Some(packet.pts);
        for i in 0..self.audio.len() {
            let waiting = std::mem::take(&mut self.audio[i].before_video);
            for p in &waiting {
                self.accept_audio(i, p)?;
            }
        }
        self.push_video(0, 0, packet, converted.sample, size);
        Ok(())
    }

    fn push_video(&mut self, rel: Hns, ts: i64, packet: &Packet, sample: Vec<u8>, size: u32) {
        let v = &mut self.video;
        v.data.extend_from_slice(&sample);
        v.samples.push(VideoSample {
            ts,
            size,
            sync: packet.keyframe,
        });
        v.prev_rel = Some(v.last_rel);
        v.last_rel = rel;
        v.last_ts = ts;
        v.last_duration = packet.duration;
    }

    fn write_audio(&mut self, i: usize, packet: &Packet) -> Result<(), MuxError> {
        if packet.data.is_empty() {
            return Err(MuxError::malformed("audio packet", "empty access unit"));
        }
        if self.base.is_none() {
            let q = &mut self.audio[i].before_video;
            if q.len() == MAX_AUDIO_BEFORE_VIDEO {
                q.pop_front();
            }
            q.push_back(packet.clone());
            return Ok(());
        }
        self.accept_audio(i, packet)
    }

    /// Places one audio packet on its track timeline.
    ///
    /// Frames before the clip start are dropped. AAC frames have a fixed
    /// length, so frames are laid out back to back and the packet timestamp
    /// is only used to detect gaps: a frame within one frame length of where
    /// the previous one ended snaps to it (this hides capture clock jitter), a
    /// later one opens a gap in the timeline, and an earlier one is a duplicate
    /// that is dropped. The first frame is placed at its own time; a track
    /// that starts late simply has a later first `tfdt`.
    fn accept_audio(&mut self, i: usize, packet: &Packet) -> Result<(), MuxError> {
        let Some(base) = self.base else {
            return Ok(());
        };
        let rel = packet
            .pts
            .checked_sub(base)
            .ok_or_else(|| MuxError::malformed("audio packet", "timestamp out of range"))?;
        let t = &mut self.audio[i];
        let at = to_ticks(rel, t.sample_rate);
        if at < 0 {
            return Ok(());
        }
        let dts = match t.next_dts {
            None => at,
            Some(next) => {
                let diff = at - next;
                if diff.abs() <= AAC_FRAME {
                    next
                } else if diff > 0 {
                    at
                } else {
                    return Ok(());
                }
            }
        };
        t.next_dts = Some(dts + AAC_FRAME);
        t.pending.push(AudioSample {
            dts,
            data: Arc::clone(&packet.data),
        });
        Ok(())
    }

    fn flush_checked(&mut self, end: End) -> Result<(), MuxError> {
        self.flush(end).inspect_err(|_| self.poisoned = true)
    }

    /// Writes the open GOP as one `moof` + `mdat`.
    fn flush(&mut self, end: End) -> Result<(), MuxError> {
        if self.video.samples.is_empty() {
            return Ok(());
        }
        // Video: durations come from the next sample's timestamp; the last
        // one ends where the next fragment starts (or at the end of the clip).
        let last = self.video.samples.last().map_or(0, |s| s.ts);
        let (end_rel, end_ticks) = match end {
            End::Keyframe(rel) => (rel, to_ticks(rel, VIDEO_TIMESCALE)),
            End::Final => {
                let rel = self.final_video_end();
                (rel, to_ticks(rel, VIDEO_TIMESCALE).max(last + 1))
            }
        };
        let mut video_entries = Vec::with_capacity(self.video.samples.len());
        for (i, s) in self.video.samples.iter().enumerate() {
            let next = self.video.samples.get(i + 1).map_or(end_ticks, |n| n.ts);
            video_entries.push(TrunSample {
                duration: u32::try_from(next - s.ts)
                    .map_err(|_| MuxError::TooLarge("video sample duration"))?,
                size: s.size,
                flags: if s.sync { FLAGS_SYNC } else { FLAGS_NON_SYNC },
            });
        }
        let video_start = u64::try_from(self.video.samples[0].ts).unwrap_or(0);

        // Audio: everything that starts before the fragment's end time.
        struct AudioFragment {
            file_id: u32,
            start: u64,
            entries: Vec<TrunSample>,
            data: Vec<u8>,
            media_end: u64,
        }
        let mut audio_frags: Vec<(usize, AudioFragment)> = Vec::new();
        for (i, a) in self.audio.iter_mut().enumerate() {
            let bound = to_ticks(end_rel, a.sample_rate);
            let take = a.pending.partition_point(|s| s.dts < bound);
            if take == 0 {
                if matches!(end, End::Final) {
                    a.pending.clear();
                }
                continue;
            }
            let mut entries = Vec::with_capacity(take);
            let mut data = Vec::new();
            for k in 0..take {
                let s = &a.pending[k];
                let next = a.pending.get(k + 1).map(|n| n.dts);
                let duration = next.map_or(AAC_FRAME, |n| n - s.dts);
                entries.push(TrunSample {
                    duration: u32::try_from(duration)
                        .map_err(|_| MuxError::TooLarge("audio sample duration"))?,
                    size: u32::try_from(s.data.len())
                        .map_err(|_| MuxError::TooLarge("audio sample"))?,
                    flags: 0,
                });
                data.extend_from_slice(&s.data);
            }
            let first = a.pending[0].dts;
            let last_dts = a.pending[take - 1].dts;
            let frag = AudioFragment {
                file_id: i as u32 + 2,
                start: u64::try_from(first).unwrap_or(0),
                entries,
                data,
                media_end: u64::try_from(last_dts + AAC_FRAME).unwrap_or(0),
            };
            if matches!(end, End::Final) {
                a.pending.clear();
            } else {
                a.pending.drain(..take);
            }
            audio_frags.push((i, frag));
        }

        let mut trafs = vec![TrafSpec {
            track_id: 1,
            base_media_decode_time: video_start,
            per_sample_flags: true,
            samples: &video_entries,
        }];
        for (_, f) in &audio_frags {
            trafs.push(TrafSpec {
                track_id: f.file_id,
                base_media_decode_time: f.start,
                per_sample_flags: false,
                samples: &f.entries,
            });
        }
        let payload: u64 = trafs.iter().map(TrafSpec::payload_len).sum();
        let too_large = || MuxError::TooLarge("fragment");
        if payload + 8 > u64::from(u32::MAX) {
            return Err(too_large());
        }
        self.fragments += 1;
        let moof = build_moof(self.fragments, &trafs).ok_or_else(too_large)?;
        let mut mdat_header = [0u8; 8];
        mdat_header[..4].copy_from_slice(&((payload + 8) as u32).to_be_bytes());
        mdat_header[4..].copy_from_slice(b"mdat");

        self.out.write_all(&moof)?;
        self.out.write_all(&mdat_header)?;
        self.out.write_all(&self.video.data)?;
        self.pos += (moof.len() + mdat_header.len() + self.video.data.len()) as u64;
        for (_, f) in &audio_frags {
            self.out.write_all(&f.data)?;
            self.pos += f.data.len() as u64;
        }

        self.video.media_end = u64::try_from(end_ticks).unwrap_or(0);
        for (i, f) in &audio_frags {
            self.audio[*i].media_end = f.media_end;
        }
        self.video.data.clear();
        self.video.samples.clear();
        Ok(())
    }

    /// Clip-relative end of the last video sample.
    fn final_video_end(&self) -> Hns {
        let v = &self.video;
        let duration = if v.last_duration > 0 {
            v.last_duration
        } else if let Some(prev) = v.prev_rel {
            v.last_rel - prev
        } else {
            HNS_PER_SEC / Hns::from(v.format.fps.max(1))
        };
        v.last_rel.saturating_add(duration)
    }

    fn write_init(&mut self) -> Result<(), MuxError> {
        let Some(config) = &self.config else {
            return Err(MuxError::NoVideo);
        };
        let mut tracks = vec![InitTrack {
            track_id: 1,
            name: &self.video.name,
            timescale: VIDEO_TIMESCALE,
            enabled: true,
            kind: TrackKind::Video {
                width: self.video.format.width as u16,
                height: self.video.format.height as u16,
                config,
                color: self.video.format.color,
            },
        }];
        for (i, a) in self.audio.iter().enumerate() {
            tracks.push(InitTrack {
                track_id: i as u32 + 2,
                name: &a.name,
                timescale: a.sample_rate,
                enabled: a.enabled,
                kind: TrackKind::Audio {
                    channels: a.channels,
                    sample_rate: a.sample_rate,
                    audio_specific_config: &a.config,
                },
            });
        }
        let creation = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| clamp_u32(d.as_secs() + MP4_EPOCH_OFFSET));
        let (init, patches) = build_init(&tracks, creation)?;
        self.out.write_all(&init)?;
        self.pos += init.len() as u64;
        self.patches = Some(patches);
        Ok(())
    }

    fn patch_durations(&mut self) -> Result<(), MuxError> {
        let Some(patches) = self.patches.take() else {
            return Err(MuxError::NoVideo);
        };
        let end = self.out.stream_position()?;
        let mut writes: Vec<(u64, u32)> = Vec::new();

        let video_media = self.video.media_end;
        let video_movie = to_movie_units(video_media, VIDEO_TIMESCALE);
        let mut movie_duration = video_movie;
        writes.push((patches.tracks[0].tkhd_duration, clamp_u32(video_movie)));
        writes.push((patches.tracks[0].mdhd_duration, clamp_u32(video_media)));
        for (a, p) in self.audio.iter().zip(&patches.tracks[1..]) {
            let movie = to_movie_units(a.media_end, a.sample_rate);
            movie_duration = movie_duration.max(movie);
            writes.push((p.tkhd_duration, clamp_u32(movie)));
            writes.push((p.mdhd_duration, clamp_u32(a.media_end)));
        }
        writes.push((patches.mvhd_duration, clamp_u32(movie_duration)));
        writes.push((patches.mehd_duration, clamp_u32(movie_duration)));

        for (pos, value) in writes {
            self.out.seek(SeekFrom::Start(pos))?;
            self.out.write_all(&value.to_be_bytes())?;
        }
        self.out.seek(SeekFrom::Start(end))?;
        Ok(())
    }
}

fn non_empty(name: &str, default: &str) -> String {
    if name.is_empty() {
        default.to_owned()
    } else {
        name.to_owned()
    }
}

/// Muxes a complete snapshot into `out`. Equivalent to [`Mp4Writer::new`],
/// one [`Mp4Writer::write`] per packet, then [`Mp4Writer::finish`].
pub fn write_clip<W: Write + Seek>(
    out: W,
    tracks: Vec<TrackInfo>,
    packets: &[Packet],
) -> Result<W, MuxError> {
    let mut writer = Mp4Writer::new(out, tracks)?;
    for p in packets {
        writer.write(p)?;
    }
    writer.finish()
}
