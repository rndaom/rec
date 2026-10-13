//! Fragmented MP4 writer for clips saved from the replay buffer.
//!
//! Pure Rust, no dependencies beyond `rec-core`. The input is the packet
//! stream described in [`rec_core::media`]; the output is an MP4 with one
//! `moof`+`mdat` fragment per GOP, so a file that is cut off (crash, power
//! loss, full disk) still plays up to its last complete fragment.
//!
//! # Layout
//!
//! * Time zero is the pts of the first video keyframe. Video before it and
//!   audio that starts before it are dropped. A track's timestamps are
//!   converted from absolute 100 ns ticks to the track timescale, never
//!   accumulated, so rounding cannot drift.
//! * Video timescale is [`VIDEO_TIMESCALE`] (90 kHz); an audio track's
//!   timescale is its sample rate, one AAC frame being 1024 ticks. The movie
//!   timescale is 1000.
//! * Audio is aligned with the video through `tfdt` alone: each track's
//!   first fragment starts at the time of its first frame, so audio that
//!   begins after time zero is simply offset. There are no edit lists, which
//!   keeps truncated files and every player's timeline handling simple.
//! * A fragment is written whenever the next video keyframe arrives, holding
//!   the video samples of the previous GOP and every audio frame that starts
//!   before that keyframe. `ftyp`+`moov` are written when the first keyframe
//!   arrives, and the durations in `mvhd`/`mehd`/`tkhd`/`mdhd` are patched in
//!   [`Mp4Writer::finish`].
//! * At [`Mp4Writer::finish`] the clip ends with the last video sample; audio
//!   that starts after it (audio runs slightly ahead of video) is cut.
//! * The video `dts` and `duration` of packets are ignored: with no B-frames
//!   `pts == dts` and a sample lasts until the next one.

mod annexb;
mod av1;
mod bits;
mod boxes;
mod error;
mod fragment;
mod h264;
mod h265;
mod init;
mod video;
mod writer;

pub use error::MuxError;
pub use writer::{Mp4Writer, VIDEO_TIMESCALE, write_clip};
