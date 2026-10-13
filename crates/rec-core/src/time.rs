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

/// Converts a raw `QueryPerformanceCounter` value to [`Hns`] without
/// overflowing for very long uptimes.
pub const fn qpc_to_hns(counter: i64, frequency: i64) -> Hns {
    let whole = counter / frequency;
    let rest = counter % frequency;
    whole * HNS_PER_SEC + rest * HNS_PER_SEC / frequency
}

/// Current time on the QPC clock in [`Hns`]. This is the same clock as WASAPI
/// `u64QPCPosition` and Windows.Graphics.Capture `SystemRelativeTime`, so it is
/// the time base for every packet.
#[cfg(windows)]
pub fn qpc_now() -> Hns {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn QueryPerformanceCounter(count: *mut i64) -> i32;
        fn QueryPerformanceFrequency(frequency: *mut i64) -> i32;
    }
    let mut counter = 0;
    let mut frequency = 0;
    // SAFETY: both pointers are valid for writes; these calls cannot fail on
    // Windows XP and later.
    unsafe {
        QueryPerformanceCounter(&mut counter);
        QueryPerformanceFrequency(&mut frequency);
    }
    qpc_to_hns(counter, frequency)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qpc_conversion_is_exact_and_overflow_safe() {
        assert_eq!(qpc_to_hns(10_000_000, 10_000_000), secs(1));
        assert_eq!(qpc_to_hns(7_500_000, 3_000_000), millis(2500));
        assert_eq!(qpc_to_hns(1, 3), 3_333_333);
        // ~100 days of uptime at 10 MHz would overflow a naive `counter * 1e7`.
        let counter = 86_400 * 100 * 10_000_000_i64;
        assert_eq!(qpc_to_hns(counter, 10_000_000), counter);
    }

    #[cfg(windows)]
    #[test]
    fn qpc_now_is_monotonic() {
        let a = qpc_now();
        let b = qpc_now();
        assert!(a > 0 && b >= a);
    }
}
