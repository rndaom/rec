//! In-memory replay buffer of *encoded* packets.
//!
//! The buffer holds compressed video and audio packets, never raw frames, so
//! two minutes of 1440p video costs hundreds of megabytes rather than tens of
//! gigabytes. Invariants:
//!
//! * When non-empty, the oldest *video* packet is a keyframe, so any snapshot
//!   can be remuxed straight to a file without re-encoding.
//! * Eviction removes whole GOPs (keyframe to next keyframe) from the oldest
//!   end, and only while the remainder still covers the configured duration or
//!   the byte budget is exceeded. A single GOP larger than the byte budget is
//!   kept; GOPs are about one second long, so the overshoot is bounded.
//! * Eviction keeps audio whose timestamp is at or after the new oldest
//!   keyframe even if it arrived before it (video encoding has more latency
//!   than audio), so clips covering the whole buffer don't start silent.
//!
//! The buffer is not synchronised; `recd` owns it behind a mutex. Snapshots
//! clone `Arc`s, so taking one is cheap and the saver thread can mux it while
//! capture keeps pushing.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::time::Hns;

/// Identifies a stream inside the buffer (video, game audio, mic, ...).
pub type TrackId = u8;

#[derive(Clone, Debug)]
pub struct Packet {
    pub track: TrackId,
    /// Encoder configuration generation. It changes whenever the encoder is
    /// rebuilt (resolution, codec or device change); the saver must start a new
    /// file or init segment when it sees a different value.
    pub segment: u32,
    pub pts: Hns,
    pub dts: Hns,
    pub duration: Hns,
    pub keyframe: bool,
    pub data: Arc<[u8]>,
}

impl Packet {
    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// How much history to keep, measured on the video track.
    pub duration: Hns,
    /// Upper bound on buffered payload bytes (all tracks).
    pub max_bytes: usize,
}

#[derive(Debug)]
pub struct ReplayBuffer {
    video_track: TrackId,
    limits: Limits,
    packets: VecDeque<Packet>,
    /// Sequence number of `packets[0]`; sequence numbers grow by one per push.
    front_seq: u64,
    /// `(sequence, pts)` of every buffered video keyframe, oldest first.
    keyframes: VecDeque<(u64, Hns)>,
    bytes: usize,
    newest_video_pts: Option<Hns>,
}

impl ReplayBuffer {
    pub fn new(video_track: TrackId, limits: Limits) -> Self {
        Self {
            video_track,
            limits,
            packets: VecDeque::new(),
            front_seq: 0,
            keyframes: VecDeque::new(),
            bytes: 0,
            newest_video_pts: None,
        }
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    pub fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
        self.evict();
    }

    pub fn len(&self) -> usize {
        self.packets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    /// Buffered payload bytes across all tracks.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Video time covered, from the oldest keyframe to the newest video packet.
    pub fn buffered_duration(&self) -> Hns {
        match (self.keyframes.front(), self.newest_video_pts) {
            (Some(&(_, first)), Some(newest)) => (newest - first).max(0),
            _ => 0,
        }
    }

    pub fn clear(&mut self) {
        self.front_seq += self.packets.len() as u64;
        self.packets.clear();
        self.keyframes.clear();
        self.bytes = 0;
        self.newest_video_pts = None;
    }

    /// Appends a packet in arrival order. Packets that arrive before the first
    /// video keyframe are dropped, because no clip could start with them.
    pub fn push(&mut self, packet: Packet) {
        let is_video = packet.track == self.video_track;
        if self.keyframes.is_empty() && !(is_video && packet.keyframe) {
            return;
        }

        let seq = self.front_seq + self.packets.len() as u64;
        if is_video {
            if packet.keyframe {
                self.keyframes.push_back((seq, packet.pts));
            }
            self.newest_video_pts = Some(match self.newest_video_pts {
                Some(newest) => newest.max(packet.pts),
                None => packet.pts,
            });
        }
        self.bytes += packet.len();
        self.packets.push_back(packet);
        self.evict();
    }

    fn evict(&mut self) {
        while self.keyframes.len() >= 2 {
            let (next_seq, next_pts) = self.keyframes[1];
            let newest = self.newest_video_pts.unwrap_or(next_pts);
            let over_budget = self.bytes > self.limits.max_bytes;
            let still_covers_duration = newest - next_pts >= self.limits.duration;
            if !(over_budget || still_covers_duration) {
                break;
            }

            let drop_count = (next_seq - self.front_seq) as usize;
            let mut kept = Vec::new();
            for packet in self.packets.drain(..drop_count) {
                if packet.track != self.video_track && packet.pts >= next_pts {
                    kept.push(packet);
                } else {
                    self.bytes -= packet.len();
                }
            }
            // Kept packets take the sequence numbers just before the new
            // oldest keyframe; only video sequence numbers are ever compared.
            self.front_seq = next_seq - kept.len() as u64;
            for packet in kept.into_iter().rev() {
                self.packets.push_front(packet);
            }
            self.keyframes.pop_front();
        }
    }

    /// Returns the packets for a clip covering `start..end` (`end = None` means
    /// "up to now"), in arrival order.
    ///
    /// Video starts at the newest keyframe at or before `start` (or the oldest
    /// keyframe if `start` predates the buffer). Other tracks are selected by
    /// timestamp from that keyframe on, which also catches audio that arrived
    /// before the keyframe because video encoding has more latency than audio.
    pub fn snapshot(&self, start: Hns, end: Option<Hns>) -> Vec<Packet> {
        let Some(&(first_seq, first_pts)) = self.keyframes.front() else {
            return Vec::new();
        };
        let (kf_seq, kf_pts) = self
            .keyframes
            .iter()
            .rev()
            .find(|&&(_, pts)| pts <= start)
            .copied()
            .unwrap_or((first_seq, first_pts));

        let before_end = |pts: Hns| end.is_none_or(|end| pts < end);
        self.packets
            .iter()
            .zip(self.front_seq..)
            .filter(|(packet, seq)| {
                if packet.track == self.video_track {
                    *seq >= kf_seq && before_end(packet.pts)
                } else {
                    packet.pts >= kf_pts && before_end(packet.pts)
                }
            })
            .map(|(packet, _)| packet.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::{millis, secs};

    const VIDEO: TrackId = 0;
    const AUDIO: TrackId = 1;

    fn packet(track: TrackId, pts: Hns, keyframe: bool, size: usize) -> Packet {
        Packet {
            track,
            segment: 0,
            pts,
            dts: pts,
            duration: millis(100),
            keyframe,
            data: vec![0u8; size].into(),
        }
    }

    /// Pushes video at 10 fps with a keyframe every second, `0..secs` seconds.
    fn fill_video(buf: &mut ReplayBuffer, seconds: i64, size: usize) {
        for i in 0..seconds * 10 {
            buf.push(packet(VIDEO, millis(i * 100), i % 10 == 0, size));
        }
    }

    fn unlimited(duration: Hns) -> Limits {
        Limits {
            duration,
            max_bytes: usize::MAX,
        }
    }

    #[test]
    fn drops_packets_before_first_keyframe() {
        let mut buf = ReplayBuffer::new(VIDEO, unlimited(secs(10)));
        buf.push(packet(AUDIO, 0, false, 10));
        buf.push(packet(VIDEO, millis(100), false, 10));
        assert!(buf.is_empty());

        buf.push(packet(VIDEO, millis(200), true, 10));
        buf.push(packet(AUDIO, millis(210), false, 10));
        assert_eq!(buf.len(), 2);
        assert_eq!(buf.bytes(), 20);
    }

    #[test]
    fn evicts_whole_gops_but_keeps_requested_duration() {
        let mut buf = ReplayBuffer::new(VIDEO, unlimited(secs(3)));
        fill_video(&mut buf, 6, 1);

        let all = buf.snapshot(Hns::MIN, None);
        assert!(all[0].keyframe);
        assert_eq!(all[0].pts, secs(2));
        assert!(buf.buffered_duration() >= secs(3));
        assert!(buf.buffered_duration() < secs(4));
    }

    #[test]
    fn byte_budget_evicts_oldest_gop() {
        let mut buf = ReplayBuffer::new(
            VIDEO,
            Limits {
                duration: secs(60),
                max_bytes: 1500,
            },
        );
        fill_video(&mut buf, 3, 100);

        // Each GOP is 10 x 100 bytes; only the newest GOP fits.
        assert_eq!(buf.bytes(), 1000);
        assert_eq!(buf.snapshot(Hns::MIN, None)[0].pts, secs(2));
    }

    #[test]
    fn shrinking_limits_evicts_immediately() {
        let mut buf = ReplayBuffer::new(VIDEO, unlimited(secs(60)));
        fill_video(&mut buf, 10, 1);
        buf.set_limits(unlimited(secs(2)));
        assert_eq!(buf.snapshot(Hns::MIN, None)[0].pts, secs(7));
    }

    #[test]
    fn snapshot_starts_at_keyframe_at_or_before_start() {
        let mut buf = ReplayBuffer::new(VIDEO, unlimited(secs(60)));
        fill_video(&mut buf, 6, 1);

        let clip = buf.snapshot(millis(2500), None);
        assert!(clip[0].keyframe);
        assert_eq!(clip[0].pts, secs(2));
        assert_eq!(clip.last().unwrap().pts, millis(5900));

        let before_buffer = buf.snapshot(-secs(5), None);
        assert_eq!(before_buffer[0].pts, 0);
    }

    #[test]
    fn snapshot_selects_audio_by_timestamp_not_arrival() {
        let mut buf = ReplayBuffer::new(VIDEO, unlimited(secs(60)));
        fill_video(&mut buf, 2, 1);
        // Audio for t=2.05s arrives before the delayed t=2.0s video keyframe.
        buf.push(packet(AUDIO, millis(1950), false, 1));
        buf.push(packet(AUDIO, millis(2050), false, 1));
        buf.push(packet(VIDEO, secs(2), true, 1));
        buf.push(packet(VIDEO, millis(2100), false, 1));

        let clip = buf.snapshot(secs(2), None);
        let audio: Vec<_> = clip.iter().filter(|p| p.track == AUDIO).collect();
        assert_eq!(audio.len(), 1);
        assert_eq!(audio[0].pts, millis(2050));
        assert_eq!(clip.iter().filter(|p| p.track == VIDEO).count(), 2);
    }

    #[test]
    fn eviction_keeps_early_arriving_audio_of_the_new_first_gop() {
        let mut buf = ReplayBuffer::new(VIDEO, unlimited(secs(2)));
        fill_video(&mut buf, 2, 1);
        // Audio for t=2.05s arrives before the t=2.0s keyframe.
        buf.push(packet(AUDIO, millis(1950), false, 1));
        buf.push(packet(AUDIO, millis(2050), false, 1));
        for i in 20..50 {
            buf.push(packet(VIDEO, millis(i * 100), i % 10 == 0, 1));
        }
        // Buffer now starts at the t=2s keyframe; the t=2.05s audio survived.
        let clip = buf.snapshot(Hns::MIN, None);
        let first_video = clip.iter().find(|p| p.track == VIDEO).unwrap();
        assert_eq!(first_video.pts, secs(2));
        let audio: Vec<_> = clip
            .iter()
            .filter(|p| p.track == AUDIO)
            .map(|p| p.pts)
            .collect();
        assert_eq!(audio, vec![millis(2050)]);
        assert_eq!(buf.bytes(), clip.len());

        // Keeps working as more GOPs are evicted.
        for i in 50..70 {
            buf.push(packet(VIDEO, millis(i * 100), i % 10 == 0, 1));
        }
        let clip = buf.snapshot(Hns::MIN, None);
        assert_eq!(clip[0].pts, secs(4));
        assert!(clip.iter().all(|p| p.track == VIDEO));
        assert_eq!(buf.bytes(), clip.len());
    }

    #[test]
    fn snapshot_respects_end_bound() {
        let mut buf = ReplayBuffer::new(VIDEO, unlimited(secs(60)));
        fill_video(&mut buf, 6, 1);
        let clip = buf.snapshot(secs(1), Some(secs(3)));
        assert_eq!(clip.first().unwrap().pts, secs(1));
        assert_eq!(clip.last().unwrap().pts, millis(2900));
    }

    #[test]
    fn snapshot_shares_payloads() {
        let mut buf = ReplayBuffer::new(VIDEO, unlimited(secs(60)));
        fill_video(&mut buf, 1, 64);
        let a = buf.snapshot(Hns::MIN, None);
        let b = buf.snapshot(Hns::MIN, None);
        assert!(Arc::ptr_eq(&a[0].data, &b[0].data));
    }

    #[test]
    fn clear_resets_state() {
        let mut buf = ReplayBuffer::new(VIDEO, unlimited(secs(60)));
        fill_video(&mut buf, 2, 8);
        buf.clear();
        assert!(buf.is_empty());
        assert_eq!(buf.bytes(), 0);
        assert_eq!(buf.buffered_duration(), 0);
        assert!(buf.snapshot(Hns::MIN, None).is_empty());

        // Usable again after clearing, starting from a new keyframe.
        buf.push(packet(VIDEO, secs(9), false, 1));
        assert!(buf.is_empty());
        buf.push(packet(VIDEO, secs(10), true, 1));
        assert_eq!(buf.len(), 1);
    }
}
