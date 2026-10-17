//! `summary.md`: a clip in plain language, for a person or a model reading
//! a `rec digest` bundle.

use std::fmt::Write;

use crate::clip::{Clip, Source};
use crate::overlap::Overlap;
use crate::time::{clock, format_local, format_utc};
use crate::timeline::{Entry, db};

/// A still in the bundle.
#[derive(Clone, Debug, PartialEq)]
pub struct Still {
    pub t: f64,
    /// Relative to the bundle, e.g. `frames/t021.18.jpg`.
    pub file: String,
    /// Why it was taken: "marker (hotkey)", "every 5 s", ….
    pub reasons: Vec<String>,
}

/// A contact sheet in the bundle.
#[derive(Clone, Debug, PartialEq)]
pub struct Sheet {
    pub file: String,
    pub count: usize,
    pub from: f64,
    pub to: f64,
}

/// What the summary covers besides the clip and its timeline.
#[derive(Debug, Default)]
pub struct Extras<'a> {
    /// The clip's place among the session's saved clips (1-based).
    pub position: Option<(usize, usize)>,
    /// Overlapping clips, with their file names.
    pub overlaps: Vec<(&'a str, &'a Overlap)>,
    pub group: Option<&'a str>,
    pub stills: &'a [Still],
    pub still_width: u32,
    pub sheets: &'a [Sheet],
    /// Why there are no stills, if there are none.
    pub no_stills: Option<String>,
}

pub fn render(clip: &Clip, entries: &[Entry], extras: &Extras) -> String {
    let mut md = String::new();
    let title = if clip.file.is_empty() {
        &clip.id
    } else {
        &clip.file
    };
    let _ = writeln!(md, "# {title}\n");
    facts(&mut md, clip, extras);
    timeline(&mut md, clip, entries);
    audio(&mut md, clip);
    if let Some(analysis) = &clip.analysis {
        let lines = crate::analysis::describe(analysis);
        if !lines.is_empty() {
            md.push_str("## Picture\n\n");
            for line in lines {
                let _ = writeln!(md, "- {line}");
            }
            md.push('\n');
        }
    }
    if !extras.overlaps.is_empty() {
        md.push_str("## Overlapping clips\n\n");
        if let Some(group) = extras.group {
            let _ = writeln!(md, "Overlap group `{group}` (see `index.json`).\n");
        }
        for (file, o) in &extras.overlaps {
            let _ = writeln!(
                md,
                "- `{file}` ({}): shares {}–{} of this clip ({:.1} s). Its time = this clip's time {} {:.3} s.",
                relation_text(o),
                clock(o.shared.0),
                clock(o.shared.1),
                o.seconds,
                if o.offset < 0.0 { '−' } else { '+' },
                o.offset.abs()
            );
        }
        md.push('\n');
    }
    if !extras.stills.is_empty() {
        let _ = writeln!(
            md,
            "## Stills\n\n`frames/` holds {} stills, {} px wide, named by clip time in seconds.\n\n| still | time | why |\n|---|---|---|",
            extras.stills.len(),
            extras.still_width
        );
        for s in extras.stills {
            let _ = writeln!(
                md,
                "| `{}` | {} | {} |",
                s.file,
                clock(s.t),
                s.reasons.join("; ")
            );
        }
        md.push('\n');
    } else if let Some(why) = &extras.no_stills {
        let _ = writeln!(md, "## Stills\n\nNone: {why}\n");
    }
    if !extras.sheets.is_empty() {
        md.push_str("## Contact sheets\n\nEach tile is labelled with its clip time and why it was taken.\n\n");
        for s in extras.sheets {
            let _ = writeln!(
                md,
                "- `{}`: {} stills, {}–{}",
                s.file,
                s.count,
                clock(s.from),
                clock(s.to)
            );
        }
        md.push('\n');
    }
    md.push_str(
        "## Files\n\n- `clip.rec.json`: the clip's sidecar (format: `docs/ai-clips.md` in the rec repository). Times are seconds from the clip's first frame.\n",
    );
    md
}

fn facts(md: &mut String, clip: &Clip, extras: &Extras) {
    let game = &clip.game;
    let mut what = if game.name.is_empty() {
        "unknown".to_owned()
    } else {
        game.name.clone()
    };
    match (game.exe.is_empty(), game.steam_appid) {
        (false, Some(app)) => {
            let _ = write!(what, " (`{}`, Steam app {app})", game.exe);
        }
        (false, None) => {
            let _ = write!(what, " (`{}`)", game.exe);
        }
        (true, Some(app)) => {
            let _ = write!(what, " (Steam app {app})");
        }
        (true, None) => {}
    }
    let _ = writeln!(md, "- **Game:** {what}");
    if let Some(start) = clip.start_utc {
        let local = clip
            .utc_offset_min
            .map(|off| format!("{}, ", format_local(start, off)))
            .unwrap_or_default();
        let _ = writeln!(md, "- **Recorded:** {local}{}", format_utc(start));
    }
    let mut length = format!("{:.1} s", clip.duration);
    if let Some(v) = &clip.video_info {
        let _ = write!(
            length,
            " · {}×{}, {} fps, {}, {}",
            v.width,
            v.height,
            v.fps,
            v.codec.to_uppercase(),
            if v.hdr { "HDR10" } else { "SDR" }
        );
    }
    let _ = writeln!(md, "- **Length:** {length}");
    if let Some((session, from, to)) = clip.session_range() {
        let place = extras
            .position
            .map(|(n, of)| format!("; clip {n} of {of} saved in this session"))
            .unwrap_or_default();
        let _ = writeln!(
            md,
            "- **Session:** `{session}`, this clip is {}–{} of it{place}",
            clock(from),
            clock(to)
        );
    }
    if let Some(at) = clip.saved_at {
        let by = clip
            .trigger
            .as_deref()
            .map(|t| match t {
                "cli" => "by `rec save` ".to_owned(),
                "tray" => "from the tray ".to_owned(),
                "panel" => "from the quick panel ".to_owned(),
                t => format!("by {t} "),
            })
            .unwrap_or_default();
        let _ = writeln!(
            md,
            "- **Saved:** {by}at {} (the clip runs to {})",
            clock(at),
            clock(clip.duration)
        );
    }
    if !clip.audio.is_empty() {
        let tracks: Vec<String> = clip
            .audio
            .iter()
            .map(|t| format!("{} {} ({})", t.index, t.name, t.role))
            .collect();
        let _ = writeln!(md, "- **Audio tracks:** {}", tracks.join(", "));
    }
    let _ = writeln!(md, "- **Clip id:** `{}`", clip.id);
    if clip.source == Source::Journal {
        md.push_str("- **Note:** this clip has no sidecar; these facts come from the session journal, so there are no audio or video details.\n");
    }
    md.push('\n');
}

fn timeline(md: &mut String, clip: &Clip, entries: &[Entry]) {
    md.push_str("## Timeline\n\n");
    let mut lines: Vec<(f64, String)> = entries
        .iter()
        .map(|e| {
            let when = match e.dur {
                Some(d) if d >= 0.15 => format!("{}–{}", clock(e.t), clock(e.t + d)),
                _ => clock(e.t),
            };
            let mut line = format!("- **{when}** {}", e.note);
            if e.kind == "loud" {
                let _ = write!(line, " (`{}`, conf {:.2})", e.src, e.conf);
            }
            (e.t, line)
        })
        .collect();
    if let Some(at) = clip.saved_at {
        lines.push((at, format!("- **{}** save requested", clock(at))));
    }
    lines.sort_by(|a, b| a.0.total_cmp(&b.0));
    if lines.is_empty() {
        md.push_str("Nothing marked or detected.\n");
    }
    for (_, line) in lines {
        md.push_str(&line);
        md.push('\n');
    }
    if !entries.iter().any(|e| e.kind == "marker") {
        md.push_str("\nNo markers in this clip.\n");
    }
    md.push('\n');
}

fn audio(md: &mut String, clip: &Clip) {
    let described: Vec<String> = clip
        .audio
        .iter()
        .filter(|t| !t.loudness.is_empty())
        .map(|t| {
            let mut sorted = t.loudness.clone();
            sorted.sort_unstable();
            let median = db(sorted[sorted.len() / 2]);
            let (peak_i, peak) = (t.loudness.iter().enumerate())
                .max_by_key(|(i, v)| (**v, std::cmp::Reverse(*i)))
                .map_or((0, 0), |(i, v)| (i, *v));
            let silent = t.loudness.iter().filter(|&&v| v == 0).count() as f64 / t.loudness.len() as f64;
            if peak == 0 {
                format!("- {} ({}): silent or muted throughout", t.name, t.role)
            } else {
                format!(
                    "- {} ({}): median {:.0} dBFS, loudest {:.0} dBFS at {}, silent {:.0}% of the time",
                    t.name,
                    t.role,
                    median,
                    db(peak),
                    clock((peak_i as f64 + 0.5) / f64::from(clip.hz)),
                    silent * 100.0
                )
            }
        })
        .collect();
    if described.is_empty() {
        return;
    }
    md.push_str("## Audio\n\nRMS level per 100 ms (`loudness` in the sidecar).\n\n");
    for line in described {
        md.push_str(&line);
        md.push('\n');
    }
    md.push('\n');
}

fn relation_text(o: &Overlap) -> &'static str {
    use crate::overlap::Relation;
    match o.relation {
        Relation::Same => "the same span",
        Relation::Contains => "inside this clip",
        Relation::Inside => "covers all of this clip",
        Relation::Partial => "partial overlap",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::tests::sample;
    use crate::overlap::Relation;
    use crate::timeline;

    #[test]
    fn renders_the_main_sections() {
        let clip = Clip::from_value(sample()).unwrap();
        let entries = timeline::timeline(&clip);
        let overlap = Overlap {
            other: 1,
            seconds: 10.0,
            offset: -20.0,
            shared: (20.0, 30.0),
            relation: Relation::Partial,
        };
        let stills = [Still {
            t: 21.18,
            file: "frames/t021.18.jpg".into(),
            reasons: vec!["marker (rec mark)".into()],
        }];
        let sheets = [Sheet {
            file: "sheet-1.jpg".into(),
            count: 1,
            from: 21.18,
            to: 21.18,
        }];
        let extras = Extras {
            position: Some((3, 7)),
            overlaps: vec![("other.mp4", &overlap)],
            group: Some("g1"),
            stills: &stills,
            still_width: 1024,
            sheets: &sheets,
            no_stills: None,
        };
        let md = render(&clip, &entries, &extras);
        for want in [
            "# Team Fortress 2_2026-10-17_21-04-33.mp4",
            "- **Game:** Team Fortress 2 (`tf_win64.exe`, Steam app 440)",
            "- **Recorded:** 2026-10-17 21:04:03 (UTC-04:00), 2026-10-18T01:04:03.112Z",
            "- **Length:** 30.0 s · 2560×1440, 60 fps, HEVC, SDR",
            "this clip is 30:43.2–31:13.2 of it; clip 3 of 7 saved in this session",
            "- **Saved:** by hotkey at 0:27.5 (the clip runs to 0:30.0)",
            "- **0:21.2** marker (rec mark)",
            "- **0:27.5** save requested",
            "- Game (game): median 0 dBFS, loudest 0 dBFS at 0:00.2, silent 33% of the time",
            "`other.mp4` (partial overlap): shares 0:20.0–0:30.0 of this clip (10.0 s). Its time = this clip's time − 20.000 s.",
            "| `frames/t021.18.jpg` | 0:21.2 | marker (rec mark) |",
            "- `sheet-1.jpg`: 1 stills, 0:21.2–0:21.2",
        ] {
            assert!(md.contains(want), "missing {want:?} in\n{md}");
        }
        // The empty microphone curve is not described.
        assert!(!md.contains("Microphone (mic):"), "{md}");
    }
}
