//! Sums several inputs into one stream on a shared sample grid.
//!
//! This module is pure (no devices, no clock reads) so every rule is
//! unit-tested.
//!
//! Every input's [`Timeline`](crate::timeline::Timeline) already places its
//! samples on the QPC clock, so the inputs only have to be lined up, not
//! resampled. The mixer works on a *grid*: grid index `g` is the sample that
//! plays at `epoch + samples_to_hns(g)`. An input hands over runs of samples
//! with the grid index of their first sample; a run that starts within
//! [`SNAP`] samples of where the input's previous run ended is treated as
//! continuous, so rounding a timestamp to the grid can never duplicate or drop
//! a sample at a run boundary.
//!
//! [`Mixer::mix`] outputs everything every input has covered (the *horizon*).
//! Inputs normally advance together, because the capture thread fills
//! silence for idle inputs on every wake-up. An input that falls more than
//! [`MAX_LAG`] behind the others is treated as silent for that range, so it
//! can never stall the mix. A stretch where no input has any data and that is
//! longer than `max_gap` (system sleep) is skipped instead of filled; the
//! caller turns the skip into a timestamp jump on the output track.

use std::collections::VecDeque;

/// Runs starting this close to the end of the previous one are continuous.
pub const SNAP: i64 = 2;
/// An input more than this many samples behind the most advanced one no
/// longer holds back the mix (1 s).
pub const MAX_LAG: i64 = 48_000;

/// A run of interleaved stereo samples starting at grid index `start`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Run {
    start: i64,
    pcm: Vec<i16>,
}

impl Run {
    fn frames(&self) -> i64 {
        (self.pcm.len() / 2) as i64
    }

    fn end(&self) -> i64 {
        self.start + self.frames()
    }
}

#[derive(Clone, Debug, Default)]
struct Input {
    runs: VecDeque<Run>,
    /// Grid index just after the last sample ever pushed (`None` before the
    /// first push).
    end: Option<i64>,
}

/// Samples handed to [`Mixer`].
#[derive(Clone, Copy, Debug)]
enum Data<'a> {
    Pcm(&'a [i16]),
    Silence(usize),
}

impl Data<'_> {
    fn frames(&self) -> i64 {
        match self {
            Data::Pcm(pcm) => (pcm.len() / 2) as i64,
            Data::Silence(frames) => *frames as i64,
        }
    }

    fn skip(self, frames: i64) -> Self {
        match self {
            Data::Pcm(pcm) => Data::Pcm(&pcm[frames as usize * 2..]),
            Data::Silence(n) => Data::Silence(n - frames as usize),
        }
    }

    fn append_to(self, out: &mut Vec<i16>) {
        match self {
            Data::Pcm(pcm) => out.extend_from_slice(pcm),
            Data::Silence(n) => out.resize(out.len() + n * 2, 0),
        }
    }
}

impl Input {
    fn push(&mut self, start: i64, data: Data<'_>, floor: i64) {
        let frames = data.frames();
        if frames == 0 {
            return;
        }
        let mut start = match self.end {
            Some(end) if (start - end).abs() <= SNAP => end,
            _ => start,
        };
        let mut data = data;
        // Drop whatever overlaps data we already hold or already mixed.
        let covered = self.end.unwrap_or(i64::MIN).max(floor);
        if start < covered {
            let skip = (covered - start).min(frames);
            data = data.skip(skip);
            start += skip;
        }
        let frames = data.frames();
        if frames == 0 {
            return;
        }
        match self.runs.back_mut() {
            Some(last) if last.end() == start => data.append_to(&mut last.pcm),
            _ => {
                let mut pcm = Vec::with_capacity(frames as usize * 2);
                data.append_to(&mut pcm);
                self.runs.push_back(Run { start, pcm });
            }
        }
        self.end = Some(start + frames);
    }

    /// Discards samples before `pos`.
    fn trim(&mut self, pos: i64) {
        while let Some(run) = self.runs.front_mut() {
            if run.end() <= pos {
                self.runs.pop_front();
            } else {
                if run.start < pos {
                    run.pcm.drain(..(pos - run.start) as usize * 2);
                    run.start = pos;
                }
                break;
            }
        }
    }

    fn first_start(&self) -> Option<i64> {
        self.runs.front().map(|r| r.start)
    }

    /// Adds this input's samples for `pos..pos + out.len() / 2` into `out`.
    fn add_into(&self, pos: i64, out: &mut [i32]) {
        let end = pos + (out.len() / 2) as i64;
        for run in &self.runs {
            if run.start >= end {
                break;
            }
            let from = run.start.max(pos);
            let to = run.end().min(end);
            if from >= to {
                continue;
            }
            let src = &run.pcm[(from - run.start) as usize * 2..(to - run.start) as usize * 2];
            let dst = &mut out[(from - pos) as usize * 2..(to - pos) as usize * 2];
            for (d, &s) in dst.iter_mut().zip(src) {
                *d += i32::from(s);
            }
        }
    }
}

/// What [`Mixer::mix`] produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mixed {
    /// Mixed samples starting at grid index `start`.
    Samples { start: i64, pcm: Vec<i16> },
    /// Nothing was recorded from `from` to `to`; the output jumps.
    Skip { from: i64, to: i64 },
}

#[derive(Clone, Debug)]
pub struct Mixer {
    inputs: Vec<Input>,
    /// Grid index of the next output sample; `None` until the first output.
    pos: Option<i64>,
    max_gap: i64,
}

impl Mixer {
    /// A mixer for `inputs` inputs. Silent stretches longer than `max_gap`
    /// samples are skipped.
    pub fn new(inputs: usize, max_gap: i64) -> Self {
        Self {
            inputs: vec![Input::default(); inputs],
            pos: None,
            max_gap,
        }
    }

    /// Hands over samples of `input` starting at grid index `start`.
    pub fn push(&mut self, input: usize, start: i64, pcm: &[i16]) {
        let floor = self.pos.unwrap_or(i64::MIN);
        self.inputs[input].push(start, Data::Pcm(pcm), floor);
    }

    /// Hands over `frames` frames of silence of `input` starting at grid
    /// index `start`.
    pub fn push_silence(&mut self, input: usize, start: i64, frames: usize) {
        let floor = self.pos.unwrap_or(i64::MIN);
        self.inputs[input].push(start, Data::Silence(frames), floor);
    }

    /// Grid index of the next output sample.
    pub fn position(&self) -> Option<i64> {
        self.pos
    }

    /// Mixes everything that is ready, in output order. `max_frames` bounds
    /// each `Samples` block.
    pub fn mix(&mut self, max_frames: usize) -> Vec<Mixed> {
        let mut out = Vec::new();
        let max_frames = i64::try_from(max_frames).unwrap_or(i64::MAX).max(1);
        let Some(newest) = self.inputs.iter().filter_map(|i| i.end).max() else {
            return out;
        };
        // Everything covered by every input (or abandoned by laggards).
        let horizon = self
            .inputs
            .iter()
            .map(|i| i.end.unwrap_or(i64::MIN))
            .min()
            .unwrap_or(newest)
            .max(newest - MAX_LAG);
        let mut pos = match self.pos {
            Some(pos) => pos,
            None => {
                let Some(first) = self.inputs.iter().filter_map(Input::first_start).min() else {
                    return out;
                };
                first
            }
        };
        while pos < horizon {
            // Skip a long stretch without any data at all.
            let next_data = self
                .inputs
                .iter()
                .filter_map(Input::first_start)
                .min()
                .unwrap_or(horizon);
            if next_data - pos > self.max_gap {
                out.push(Mixed::Skip {
                    from: pos,
                    to: next_data,
                });
                pos = next_data;
                continue;
            }
            let frames = (horizon - pos).min(max_frames);
            let mut acc = vec![0i32; frames as usize * 2];
            for input in &self.inputs {
                input.add_into(pos, &mut acc);
            }
            let pcm = acc
                .into_iter()
                .map(|s| s.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16)
                .collect();
            out.push(Mixed::Samples { start: pos, pcm });
            pos += frames;
            for input in &mut self.inputs {
                input.trim(pos);
            }
        }
        self.pos = Some(pos);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stereo(values: &[i16]) -> Vec<i16> {
        values.iter().flat_map(|&v| [v, v]).collect()
    }

    fn samples(out: &[Mixed]) -> (i64, Vec<i16>) {
        let mut start = None;
        let mut all = Vec::new();
        for m in out {
            match m {
                Mixed::Samples { start: s, pcm } => {
                    start.get_or_insert(*s);
                    all.extend(pcm.iter().step_by(2));
                }
                Mixed::Skip { .. } => panic!("unexpected skip"),
            }
        }
        (start.unwrap_or(0), all)
    }

    #[test]
    fn sums_aligned_inputs() {
        let mut m = Mixer::new(2, 48_000);
        m.push(0, 100, &stereo(&[1, 2, 3, 4]));
        m.push(1, 100, &stereo(&[10, 20, 30, 40]));
        assert_eq!(samples(&m.mix(1024)), (100, vec![11, 22, 33, 44]));
        assert_eq!(m.position(), Some(104));
    }

    #[test]
    fn waits_for_the_slowest_input() {
        let mut m = Mixer::new(2, 48_000);
        m.push(0, 0, &stereo(&[1, 1, 1, 1]));
        m.push(1, 0, &stereo(&[2, 2]));
        assert_eq!(samples(&m.mix(1024)), (0, vec![3, 3]));
        m.push(1, 2, &stereo(&[2, 2]));
        assert_eq!(samples(&m.mix(1024)), (2, vec![3, 3]));
    }

    #[test]
    fn offset_inputs_line_up_on_the_grid() {
        let mut m = Mixer::new(2, 48_000);
        m.push(0, 0, &stereo(&[1, 1, 1, 1, 1, 1]));
        m.push(1, 2, &stereo(&[5, 5, 5, 5]));
        assert_eq!(samples(&m.mix(1024)), (0, vec![1, 1, 6, 6, 6, 6]));
    }

    #[test]
    fn rounding_jitter_at_run_boundaries_is_absorbed() {
        let mut m = Mixer::new(1, 48_000);
        m.push(0, 0, &stereo(&[1, 2, 3]));
        // Rounded one sample early and one late: still continuous.
        m.push(0, 2, &stereo(&[4, 5]));
        m.push(0, 6, &stereo(&[6]));
        assert_eq!(samples(&m.mix(1024)), (0, vec![1, 2, 3, 4, 5, 6]));
    }

    #[test]
    fn real_gaps_are_silent_and_overlaps_dropped() {
        let mut m = Mixer::new(1, 48_000);
        m.push(0, 0, &stereo(&[1, 1]));
        m.push(0, 10, &stereo(&[2, 2]));
        let (_, out) = samples(&m.mix(1024));
        assert_eq!(out, vec![1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 2, 2]);
        // Already mixed: dropped.
        m.push(0, 5, &stereo(&[9; 7]));
        assert!(m.mix(1024).is_empty());
        m.push(0, 12, &stereo(&[3]));
        assert_eq!(samples(&m.mix(1024)), (12, vec![3]));
    }

    #[test]
    fn silence_counts_as_data() {
        let mut m = Mixer::new(2, 48_000);
        m.push_silence(0, 0, 3);
        m.push(1, 0, &stereo(&[1, 2]));
        m.push_silence(1, 2, 1);
        m.push(0, 3, &stereo(&[5]));
        m.push(1, 3, &stereo(&[5]));
        assert_eq!(samples(&m.mix(1024)), (0, vec![1, 2, 0, 10]));
    }

    #[test]
    fn clips_instead_of_wrapping() {
        let mut m = Mixer::new(2, 48_000);
        m.push(0, 0, &stereo(&[30_000, -30_000]));
        m.push(1, 0, &stereo(&[30_000, -30_000]));
        assert_eq!(samples(&m.mix(1024)).1, vec![i16::MAX, i16::MIN]);
    }

    #[test]
    fn a_stalled_input_cannot_hold_back_the_mix() {
        let mut m = Mixer::new(2, 10 * MAX_LAG);
        m.push(1, 0, &stereo(&[1]));
        m.push(0, 0, &vec![1; (MAX_LAG as usize + 100) * 2]);
        let out = m.mix(usize::MAX);
        let (start, pcm) = samples(&out);
        assert_eq!(start, 0);
        assert_eq!(pcm.len(), 100);
        assert_eq!(pcm[0], 2);
        assert_eq!(pcm[1], 1);
    }

    #[test]
    fn long_silence_everywhere_is_skipped() {
        let mut m = Mixer::new(2, 1000);
        m.push(0, 0, &stereo(&[1]));
        m.push(1, 0, &stereo(&[1]));
        m.mix(1024);
        m.push(0, 50_000, &stereo(&[1, 1]));
        m.push(1, 50_000, &stereo(&[2, 2]));
        let out = m.mix(1024);
        assert_eq!(
            out,
            vec![
                Mixed::Skip {
                    from: 1,
                    to: 50_000
                },
                Mixed::Samples {
                    start: 50_000,
                    pcm: vec![3, 3, 3, 3]
                }
            ]
        );
    }

    #[test]
    fn blocks_respect_max_frames() {
        let mut m = Mixer::new(1, 48_000);
        m.push(0, 0, &[1; 10 * 2]);
        let out = m.mix(4);
        let sizes: Vec<_> = out
            .iter()
            .map(|b| match b {
                Mixed::Samples { pcm, .. } => pcm.len() / 2,
                Mixed::Skip { .. } => 0,
            })
            .collect();
        assert_eq!(sizes, vec![4, 4, 2]);
    }
}
