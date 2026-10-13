//! Error type for the muxer.

use rec_core::replay::TrackId;
use rec_core::time::Hns;

/// Everything that can go wrong while muxing. The writer never panics on bad
/// input; it returns one of these instead.
#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// The track list given to [`Mp4Writer::new`](crate::Mp4Writer::new) cannot be muxed.
    #[error("invalid track configuration: {0}")]
    InvalidTracks(String),

    #[error("packet for unknown track {0}")]
    UnknownTrack(TrackId),

    /// Packets of a second encoder configuration reached the writer. The caller
    /// must split the clip into one file per segment.
    #[error("packet segment {found} differs from the clip's segment {expected}; split the clip")]
    SegmentChange { expected: u32, found: u32 },

    /// The first video keyframe lacks the parameter sets needed for the codec
    /// configuration record.
    #[error("first keyframe is missing parameter sets: {0}")]
    MissingParameterSets(&'static str),

    /// A parameter set or access unit could not be parsed.
    #[error("malformed {what}: {detail}")]
    Malformed {
        what: &'static str,
        detail: &'static str,
    },

    #[error("video pts went backwards or repeated: {previous} then {found} (100 ns ticks)")]
    NonMonotonicVideo { previous: Hns, found: Hns },

    /// A size or duration does not fit the 32-bit fields of the format.
    #[error("{0} is too large for the MP4 format")]
    TooLarge(&'static str),

    #[error("the clip contains no video keyframe")]
    NoVideo,

    /// An earlier call failed part-way, so the output may be inconsistent.
    #[error("the writer already failed and cannot be used again")]
    Poisoned,
}

impl MuxError {
    pub(crate) fn malformed(what: &'static str, detail: &'static str) -> Self {
        MuxError::Malformed { what, detail }
    }
}
