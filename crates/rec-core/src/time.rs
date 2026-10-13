//! Timestamps.
//!
//! All media timestamps are signed 100-nanosecond ticks ("hns"). That is the
//! unit of QPC-derived capture timestamps (`SystemRelativeTime` from
//! Windows.Graphics.Capture), WASAPI `u64QPCPosition` once converted, and
//! Media Foundation sample times, so every stream shares one clock with no
//! conversion on the hot path.

/// 100-nanosecond ticks.
pub type Hns = i64;

pub const HNS_PER_SEC: Hns = 10_000_000;
pub const HNS_PER_MS: Hns = 10_000;

pub const fn secs(s: i64) -> Hns {
    s * HNS_PER_SEC
}

pub const fn millis(ms: i64) -> Hns {
    ms * HNS_PER_MS
}

/// Converts ticks to fractional seconds, for display only.
pub fn as_secs_f64(t: Hns) -> f64 {
    t as f64 / HNS_PER_SEC as f64
}
