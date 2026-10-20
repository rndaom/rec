//! Layer 2 of `docs/ai-clips.md`: what the picture does, computed after
//! saving (never in recd while a game runs, D11). The clip is decoded once,
//! small, and looked at ten times a second as a 64 × 36 grid of mean
//! colours:
//!
//! - brightness: mean luma (BT.709, 0–255) per second;
//! - motion: mean absolute change of the picture (RGB, 0–255) between looks
//!   0.1 s apart, per second;
//! - scene cuts: a jump in both the picture and its colour histogram, well
//!   above the motion around it (`cut` events, ±0.1 s). Colour, not just
//!   luma: a cut between two scenes of equal brightness still counts;
//! - dark stretches (mean luma under 20) and static ones (almost no change
//!   for 2 s: paused, a menu, loading);
//! - a thumbnail at the first marker, else the loudest moment, else the
//!   middle, moved off dark or static frames.
//!
//! Results go into the sidecar's `analysis` object and `events` (with the
//! loud moments from the loudness curves), and the thumbnail into
//! `<clip>.rec\thumb.jpg`. Running it again replaces them.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::clip::{Clip, round3};
use crate::decode::{Decoder, Frame};
use crate::image::Imaging;
use crate::time::{clock, format_utc};
use crate::timeline::loud_events;

/// Version of the `analysis` object.
pub const VERSION: u64 = 1;
/// Looks per second.
pub const LOOK_HZ: f64 = 10.0;
/// Width frames are decoded at for analysis.
pub const DECODE_WIDTH: u32 = 256;
pub const THUMB_WIDTH: u32 = 480;

const GRID_W: usize = 64;
const GRID_H: usize = 36;
const DARK: f64 = 20.0;
const STATIC_DIFF: f64 = 1.0;
const STATIC_SECONDS: f64 = 2.0;
/// Event sources analysis writes; replaced on every run.
const SOURCE: &str = "analysis:video";

/// One look at the picture.
#[derive(Clone, Debug)]
pub struct Look {
    pub t: f64,
    /// Mean B, G, R of each cell.
    grid: Vec<[u8; 3]>,
}

impl Look {
    /// The frame as a 64 × 36 grid of mean colours.
    pub fn of(frame: &Frame) -> Self {
        let (w, h) = (frame.width as usize, frame.height as usize);
        let mut grid = vec![[0u8; 3]; GRID_W * GRID_H];
        // At most 8 × 8 samples per cell, so a full-size frame stays cheap.
        for gy in 0..GRID_H {
            let (y0, y1) = (
                gy * h / GRID_H,
                ((gy + 1) * h / GRID_H).max(gy * h / GRID_H + 1),
            );
            let ystep = ((y1 - y0) / 8).max(1);
            for gx in 0..GRID_W {
                let (x0, x1) = (
                    gx * w / GRID_W,
                    ((gx + 1) * w / GRID_W).max(gx * w / GRID_W + 1),
                );
                let xstep = ((x1 - x0) / 8).max(1);
                let (mut sum, mut n) = ([0u32; 3], 0u32);
                for y in (y0..y1.min(h)).step_by(ystep) {
                    let row = &frame.bgra[y * w * 4..(y + 1) * w * 4];
                    for x in (x0..x1.min(w)).step_by(xstep) {
                        for (c, total) in sum.iter_mut().enumerate() {
                            *total += u32::from(row[x * 4 + c]);
                        }
                        n += 1;
                    }
                }
                let n = n.max(1);
                grid[gy * GRID_W + gx] = sum.map(|c| ((c + n / 2) / n) as u8);
            }
        }
        Self { t: frame.t, grid }
    }

    fn brightness(&self) -> f64 {
        let sum: f64 = (self.grid.iter())
            .map(|[b, g, r]| {
                0.0722 * f64::from(*b) + 0.7152 * f64::from(*g) + 0.2126 * f64::from(*r)
            })
            .sum();
        sum / self.grid.len() as f64
    }

    /// Mean absolute change per channel, 0–255.
    fn diff(&self, other: &Self) -> f64 {
        let sum: u64 = (self.grid.iter().zip(&other.grid))
            .flat_map(|(a, b)| (0..3).map(move |c| u64::from(a[c].abs_diff(b[c]))))
            .sum();
        sum as f64 / (3 * self.grid.len()) as f64
    }

    /// Share of cells in each of 4 × 4 × 4 colour bins.
    fn histogram(&self) -> [f64; 64] {
        let mut h = [0.0; 64];
        for [b, g, r] in &self.grid {
            h[usize::from(r >> 6) << 4 | usize::from(g >> 6) << 2 | usize::from(b >> 6)] += 1.0;
        }
        let n = self.grid.len() as f64;
        h.map(|c| c / n)
    }
}

/// What the looks add up to.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Picture {
    pub brightness: Vec<u8>,
    pub motion: Vec<u8>,
    /// `(t, conf)`.
    pub cuts: Vec<(f64, f64)>,
    pub dark: Vec<(f64, f64)>,
    pub still: Vec<(f64, f64)>,
    /// `(t, dark or static)` of each look, for picking the thumbnail.
    looks: Vec<(f64, bool)>,
}

/// Reads a clip's looks, in time order, into a [`Picture`].
pub fn picture(looks: &[Look], duration: f64) -> Picture {
    let mut p = Picture::default();
    if looks.is_empty() {
        return p;
    }
    let bright: Vec<f64> = looks.iter().map(Look::brightness).collect();
    let mut diffs = vec![0.0];
    let mut hist_jumps = vec![0.0];
    for pair in looks.windows(2) {
        diffs.push(pair[1].diff(&pair[0]));
        let (a, b) = (pair[0].histogram(), pair[1].histogram());
        hist_jumps.push(a.iter().zip(&b).map(|(x, y)| (x - y).abs()).sum::<f64>() / 2.0);
    }
    // Per-second curves.
    let seconds = (duration.max(looks.last().map_or(0.0, |l| l.t)).ceil() as usize).max(1);
    let mut sums = vec![(0.0, 0.0, 0usize); seconds];
    for (i, look) in looks.iter().enumerate() {
        let s = (look.t.floor() as usize).min(seconds - 1);
        sums[s].0 += bright[i];
        sums[s].1 += diffs[i];
        sums[s].2 += 1;
    }
    let mut last = (0.0, 0.0);
    for &(b, d, n) in &sums {
        if n > 0 {
            last = (b / n as f64, d / n as f64);
        }
        p.brightness.push(last.0.round().min(255.0) as u8);
        p.motion.push(last.1.round().min(255.0) as u8);
    }
    // Cuts: a big change in both the picture and its histogram, standing out
    // from the motion around it.
    let mut cuts: Vec<(f64, f64, f64)> = Vec::new();
    for i in 1..looks.len() {
        let lo = i.saturating_sub(10);
        let hi = (i + 11).min(looks.len());
        let mut around: Vec<f64> = (lo..hi)
            .filter(|&j| j != i && j > 0)
            .map(|j| diffs[j])
            .collect();
        around.sort_by(f64::total_cmp);
        let median = around.get(around.len() / 2).copied().unwrap_or(0.0);
        if hist_jumps[i] >= 0.35 && diffs[i] >= 20.0 && diffs[i] >= 3.0 * median {
            let conf = (0.5 + hist_jumps[i] / 2.0).clamp(0.5, 0.95);
            match cuts.last_mut() {
                // Two looks of one cut: keep the larger.
                Some(prev) if looks[i].t - prev.0 < 0.5 => {
                    if diffs[i] > prev.2 {
                        *prev = (looks[i].t, conf, diffs[i]);
                    }
                }
                _ => cuts.push((looks[i].t, conf, diffs[i])),
            }
        }
    }
    p.cuts = cuts
        .into_iter()
        .map(|(t, conf, _)| (round3(t), (conf * 100.0).round() / 100.0))
        .collect();
    let step = 1.0 / LOOK_HZ;
    p.dark = runs(looks, |i| bright[i] < DARK, 1.0, step);
    // A cut into a still picture starts it; the look of the cut itself is
    // not part of it.
    p.still = runs(
        looks,
        |i| i > 0 && diffs[i] < STATIC_DIFF,
        STATIC_SECONDS,
        step,
    );
    p.looks = looks
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let in_range = |r: &[(f64, f64)]| r.iter().any(|&(a, b)| (a..=b).contains(&l.t));
            (l.t, bright[i] < DARK || in_range(&p.still))
        })
        .collect();
    p
}

/// `[from, to]` spans of consecutive looks where `hit` holds, at least
/// `min` seconds long.
fn runs(looks: &[Look], hit: impl Fn(usize) -> bool, min: f64, step: f64) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    let mut start: Option<f64> = None;
    for (i, look) in looks.iter().enumerate() {
        match (hit(i), start) {
            (true, None) => start = Some(look.t),
            (false, Some(s)) => {
                let end = looks[i - 1].t + step;
                if end - s >= min - 1e-9 {
                    out.push((round3(s), round3(end)));
                }
                start = None;
            }
            _ => {}
        }
    }
    if let (Some(s), Some(last)) = (start, looks.last()) {
        let end = last.t + step;
        if end - s >= min - 1e-9 {
            out.push((round3(s), round3(end)));
        }
    }
    out
}

impl Picture {
    /// The look nearest `t` that is neither dark nor static, else `t`.
    pub fn nice_time(&self, t: f64) -> f64 {
        self.looks
            .iter()
            .filter(|(_, bad)| !bad)
            .min_by(|a, b| (a.0 - t).abs().total_cmp(&(b.0 - t).abs()))
            .map_or(t, |(at, _)| *at)
    }
}

/// The time to take a clip's thumbnail at.
pub fn thumb_time(clip: &Clip, picture: &Picture) -> f64 {
    let marker = clip.events.iter().find(|e| e.kind == "marker").map(|e| e.t);
    let loud = || {
        loud_events(clip)
            .into_iter()
            .filter(|e| e["src"] != "audio:mic" && e["src"] != "audio:voice")
            .max_by(|a, b| {
                let p = |e: &Value| e["peak_db"].as_f64().unwrap_or(-60.0);
                p(a).total_cmp(&p(b))
            })
            .and_then(|e| e["peak_t"].as_f64())
    };
    let t = marker.or_else(loud).unwrap_or(clip.duration / 2.0);
    picture.nice_time(t.clamp(0.0, clip.duration.max(0.0)))
}

/// Analyses a clip: decodes it, and returns it with `analysis` and the
/// analysis events filled in (and the thumbnail written, if the clip has a
/// sidecar to point at it). Nothing is saved; see [`save`].
pub fn analyze(clip: &Clip, imaging: &Imaging) -> Result<Clip, String> {
    analyze_unless(clip, imaging, &|| false)
}

/// The error [`analyze_unless`] returns when `stop` asked it to.
pub const STOPPED: &str = "analysis stopped";

/// [`analyze`], giving up with [`STOPPED`] as soon as `stop` returns true,
/// so an app can make room for a game that has just started or a clip
/// about to be deleted. `stop` is asked after every frame (10 per clip
/// second), so it must be cheap.
pub fn analyze_unless(
    clip: &Clip,
    imaging: &Imaging,
    stop: &dyn Fn() -> bool,
) -> Result<Clip, String> {
    let video = clip.video.as_deref().ok_or("the clip's video is missing")?;
    let mut decoder = Decoder::open(video, Some(DECODE_WIDTH))?;
    let mut looks = Vec::new();
    let mut stopped = false;
    decoder.each(1.0 / LOOK_HZ, |frame| {
        looks.push(Look::of(&frame));
        stopped = stop();
        !stopped
    })?;
    if stopped {
        return Err(STOPPED.into());
    }
    let duration = if clip.duration > 0.0 {
        clip.duration
    } else {
        decoder.duration
    };
    let pic = picture(&looks, duration);
    let thumb = match &clip.sidecar {
        Some(sidecar) => Some(write_thumb(clip, sidecar, video, &pic, imaging)?),
        None => None,
    };

    let mut raw = clip.raw.clone();
    let mut analysis = json!({
        "v": VERSION,
        "tool": format!("rec {}", env!("CARGO_PKG_VERSION")),
        "utc": format_utc(now()),
        "hz": 1,
        "brightness": pic.brightness,
        "motion": pic.motion,
        "dark": pic.dark.iter().map(|&(a, b)| json!([a, b])).collect::<Vec<_>>(),
        "static": pic.still.iter().map(|&(a, b)| json!([a, b])).collect::<Vec<_>>(),
        "cuts": pic.cuts.len(),
        "notes": "brightness: mean luma 0-255 per second. motion: mean absolute change of the picture (RGB, 0-255) between looks 0.1 s apart, per second. dark: mean luma under 20. static: almost no change for 2 s or more. cut events are within 0.1 s.",
    });
    if let Some((t, file)) = thumb {
        analysis["thumb"] = json!({ "t": t, "file": file, "width": THUMB_WIDTH });
    }
    // Replace what an earlier run added.
    let mut events: Vec<Value> = raw["events"]
        .as_array()
        .map(|e| {
            e.iter()
                .filter(|e| {
                    let src = e["src"].as_str().unwrap_or_default();
                    src != SOURCE && !(e["kind"] == "loud" && src.starts_with("audio:"))
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    events.extend(loud_events(clip));
    events.extend(
        pic.cuts
            .iter()
            .map(|&(t, conf)| json!({ "t": t, "kind": "cut", "src": SOURCE, "conf": conf })),
    );
    events.sort_by(|a, b| {
        let t = |e: &Value| e["t"].as_f64().unwrap_or(0.0);
        t(a).total_cmp(&t(b))
    });
    raw["events"] = events.into();
    raw["analysis"] = analysis;

    let mut updated = Clip::from_value(raw)?;
    updated.source = clip.source;
    updated.video = clip.video.clone();
    updated.sidecar = clip.sidecar.clone();
    if let Some((_, from, _)) = clip.session_range()
        && updated.session_range().is_none()
    {
        updated.session = clip.session.clone();
        updated.session_offset = Some(from);
    }
    Ok(updated)
}

/// Writes an analysed clip's sidecar, replacing the old file in one step.
/// Clips without a sidecar keep the analysis in memory only: rec doesn't
/// create one against `output.sidecar = false`. The star is taken from the
/// file on disk, so starring a clip while it was being analysed sticks.
pub fn save(clip: &Clip) -> Result<(), String> {
    let Some(path) = &clip.sidecar else {
        return Ok(());
    };
    let mut raw = clip.raw.clone();
    let on_disk = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| v.get("starred").cloned());
    if let Some(starred) = on_disk {
        raw["starred"] = starred;
    }
    crate::clip::write_sidecar(path, &raw)
}

/// `<clip>.rec\thumb.jpg`; returns its time and path relative to the clip.
fn write_thumb(
    clip: &Clip,
    sidecar: &Path,
    video: &Path,
    pic: &Picture,
    imaging: &Imaging,
) -> Result<(f64, String), String> {
    let t = thumb_time(clip, pic);
    let mut decoder = Decoder::open(video, Some(THUMB_WIDTH))?;
    let frame = decoder.frame_at(t)?;
    let stem = clip.stem();
    let dir: PathBuf = sidecar.with_file_name(format!("{stem}.rec"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    imaging.save_jpeg(
        &imaging.scaled(&frame, THUMB_WIDTH)?,
        &dir.join("thumb.jpg"),
    )?;
    Ok((round3(frame.t), format!("{stem}.rec/thumb.jpg")))
}

pub use rec_core::store::compact_number_arrays;

/// Plain-language lines about an `analysis` object, for `summary.md`.
pub fn describe(analysis: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    let spans = |key: &str| -> Vec<String> {
        analysis[key]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|r| {
                        Some(format!(
                            "{}–{}",
                            clock(r[0].as_f64()?),
                            clock(r[1].as_f64()?)
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    if let Some(b) = analysis["brightness"].as_array().filter(|b| !b.is_empty()) {
        let mean = b.iter().filter_map(Value::as_f64).sum::<f64>() / b.len() as f64;
        let dark = spans("dark");
        let mut line = format!("Brightness: mean {mean:.0} of 255");
        if dark.is_empty() {
            line.push_str(", no dark stretches");
        } else {
            line.push_str(&format!("; dark at {}", dark.join(", ")));
        }
        lines.push(line);
    }
    if let Some(m) = analysis["motion"].as_array().filter(|m| !m.is_empty()) {
        let mut seconds: Vec<(usize, f64)> = m
            .iter()
            .enumerate()
            .map(|(i, v)| (i, v.as_f64().unwrap_or(0.0)))
            .collect();
        seconds.sort_by(|a, b| b.1.total_cmp(&a.1));
        let top: Vec<String> = seconds
            .iter()
            .take(3)
            .filter(|(_, v)| *v >= 2.0)
            .map(|(i, v)| format!("{} ({v:.0})", clock(*i as f64)))
            .collect();
        if top.is_empty() {
            lines.push("Motion: almost none".into());
        } else {
            lines.push(format!(
                "Most motion in the seconds starting {}",
                top.join(", ")
            ));
        }
    }
    let still = spans("static");
    if !still.is_empty() {
        lines.push(format!(
            "Static picture (paused, a menu or loading?) at {}",
            still.join(", ")
        ));
    }
    match analysis["cuts"].as_u64() {
        Some(0) => lines.push("No scene cuts".into()),
        Some(n) => lines.push(format!("{n} scene cuts (`cut` events, ±0.1 s)")),
        None => {}
    }
    if let (Some(file), Some(t)) = (
        analysis["thumb"]["file"].as_str(),
        analysis["thumb"]["t"].as_f64(),
    ) {
        lines.push(format!(
            "Thumbnail at {} (`{file}` next to the clip)",
            clock(t)
        ));
    }
    lines
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::tests::sample;

    fn look(t: f64, value: u8) -> Look {
        Look {
            t,
            grid: vec![[value; 3]; GRID_W * GRID_H],
        }
    }

    /// A ramp that slides along each look: moving (a mean change of 14),
    /// with the same histogram throughout.
    fn moving(t: f64, base: u8) -> Look {
        let shift = (t * 10.0).round() as usize;
        Look {
            t,
            grid: (0..GRID_W * GRID_H)
                .map(|i| [base + ((i % GRID_W * 4 + shift * 8) % 64) as u8; 3])
                .collect(),
        }
    }

    #[test]
    fn grid_of_a_frame() {
        // Left half black, right half white, at an odd size.
        let (w, h) = (101u32, 57u32);
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for (i, px) in bgra.chunks_mut(4).enumerate() {
            if i as u32 % w > w / 2 {
                px.fill(255);
            }
        }
        let l = Look::of(&Frame {
            t: 0.5,
            width: w,
            height: h,
            bgra,
        });
        assert_eq!(l.grid[0], [0; 3]);
        assert_eq!(l.grid[GRID_W - 1], [255; 3]);
        assert!((l.brightness() - 127.0).abs() < 8.0, "{}", l.brightness());
    }

    #[test]
    fn finds_cuts_dark_and_static() {
        let mut looks = Vec::new();
        for i in 0..100 {
            let t = f64::from(i) / 10.0;
            looks.push(match i {
                0..10 => look(t, 5),     // 1 s dark (and static)
                10..40 => moving(t, 60), // play
                40..70 => look(t, 200),  // cut to a still screen for 3 s
                _ => moving(t, 80),      // cut back to play
            });
        }
        let p = picture(&looks, 10.0);
        assert_eq!(p.brightness.len(), 10);
        assert_eq!(p.brightness[0], 5);
        assert_eq!(p.brightness[5], 200);
        assert_eq!(p.motion[5], 0);
        assert!(p.motion[2] > 0);
        let cut_times: Vec<f64> = p.cuts.iter().map(|c| c.0).collect();
        assert_eq!(cut_times, vec![1.0, 4.0, 7.0], "{:?}", p.cuts);
        assert_eq!(p.dark, vec![(0.0, 1.0)]);
        assert_eq!(p.still, vec![(4.1, 7.0)]);
        // The thumbnail moves off the still screen.
        let nice = p.nice_time(5.0);
        assert!(
            (nice - 7.0).abs() < 0.11 || (nice - 4.0).abs() < 0.11,
            "{nice}"
        );
        assert_eq!(p.nice_time(2.0), 2.0);
    }

    #[test]
    fn a_colour_change_at_equal_brightness_is_a_cut() {
        // Blue and red of about the same luma, a second each.
        let colour = |t: f64, bgr: [u8; 3]| Look {
            t,
            grid: vec![bgr; GRID_W * GRID_H],
        };
        let looks: Vec<Look> = (0..30)
            .map(|i| {
                let t = f64::from(i) / 10.0;
                colour(
                    t,
                    if (i / 10) % 2 == 0 {
                        [230, 105, 30]
                    } else {
                        [24, 51, 229]
                    },
                )
            })
            .collect();
        let p = picture(&looks, 3.0);
        assert_eq!(
            p.cuts.iter().map(|c| c.0).collect::<Vec<_>>(),
            vec![1.0, 2.0]
        );
        assert!((looks[0].brightness() - looks[10].brightness()).abs() < 15.0);
    }

    #[test]
    fn gentle_motion_is_not_a_cut() {
        let looks: Vec<Look> = (0..50).map(|i| moving(f64::from(i) / 10.0, 40)).collect();
        assert!(picture(&looks, 5.0).cuts.is_empty());
        assert_eq!(picture(&[], 5.0), Picture::default());
    }

    #[test]
    fn thumbnail_prefers_markers() {
        let clip = Clip::from_value(sample()).unwrap();
        let looks: Vec<Look> = (0..300).map(|i| moving(f64::from(i) / 10.0, 40)).collect();
        let p = picture(&looks, 30.0);
        assert_eq!(thumb_time(&clip, &p), 21.2);
        let mut s = sample();
        s["events"] = json!([]);
        let clip = Clip::from_value(s).unwrap();
        assert_eq!(thumb_time(&clip, &p), 15.0);
    }

    #[test]
    fn describes_an_analysis() {
        let a = json!({
            "brightness": [10, 100, 100], "motion": [0, 30, 5],
            "dark": [[0.0, 1.0]], "static": [[0.0, 2.5]], "cuts": 2,
            "thumb": {"t": 1.5, "file": "a.rec/thumb.jpg"}
        });
        let lines = describe(&a);
        assert_eq!(
            lines[0],
            "Brightness: mean 70 of 255; dark at 0:00.0–0:01.0"
        );
        assert_eq!(
            lines[1],
            "Most motion in the seconds starting 0:01.0 (30), 0:02.0 (5)"
        );
        assert_eq!(
            lines[2],
            "Static picture (paused, a menu or loading?) at 0:00.0–0:02.5"
        );
        assert_eq!(lines[3], "2 scene cuts (`cut` events, ±0.1 s)");
        assert!(lines[4].starts_with("Thumbnail at 0:01.5"));
    }
}
