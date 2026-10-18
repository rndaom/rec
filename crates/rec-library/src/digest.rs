//! `rec digest`: a self-contained bundle per clip that any model can read
//! without tools:
//!
//! ```text
//! <bundle>\clip.rec.json   the sidecar (with layer 2 analysis)
//! <bundle>\summary.md      the clip in plain language
//! <bundle>\frames\t021.18.jpg …  stills at each event and every N seconds
//! <bundle>\sheet-1.jpg …   a few labelled contact sheets
//! ```
//!
//! For a folder, each clip gets `<out>\<clip name>\` and `<out>\index.json`
//! lists them all with their overlap groups.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::analysis;
use crate::clip::{Clip, Source, round3};
use crate::decode::{Decoder, Frame};
use crate::image::{Imaging, SheetLayout};
use crate::library::{DIGEST_DIR, Library};
use crate::overlap;
use crate::summary::{self, Extras, Sheet, Still};
use crate::time::{clock, format_utc, still_name};
use crate::timeline::{self, Entry};

#[derive(Clone, Debug)]
pub struct Options {
    /// The bundle folder for a clip, or the index folder for a folder.
    pub out: Option<PathBuf>,
    /// A still every this many seconds, besides the events.
    pub every: f64,
    /// Width of the stills.
    pub width: u32,
    pub sheets: usize,
    pub per_sheet: usize,
    pub layout: SheetLayout,
    /// Run layer 2 analysis on clips that don't have it yet.
    pub analyze: bool,
    /// Decode stills and sheets (off: only `clip.rec.json`, `summary.md`
    /// and the index).
    pub frames: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            out: None,
            every: 5.0,
            width: 1024,
            sheets: 4,
            per_sheet: 16,
            layout: SheetLayout::default(),
            analyze: true,
            frames: true,
        }
    }
}

/// Scene cuts get stills only if a clip has this few; otherwise the
/// periodic stills show them.
const MAX_CUT_STILLS: usize = 12;
/// Planned stills closer than this become one.
const MERGE: f64 = 0.25;

/// What happened to one clip.
#[derive(Debug)]
pub struct Done {
    pub id: String,
    pub file: String,
    pub bundle: PathBuf,
    pub stills: usize,
    pub sheets: usize,
    /// Problems that didn't stop the bundle (analysis or decoding failed).
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub struct Report {
    pub done: Vec<Done>,
    /// Clips that could not be digested at all.
    pub failed: Vec<(String, String)>,
    pub index: Option<PathBuf>,
}

/// Digests a clip (its video or sidecar) or every clip under a folder.
pub fn run(
    target: &Path,
    journal_dir: &Path,
    opts: &Options,
    mut progress: impl FnMut(&str),
) -> Result<Report, String> {
    let imaging = if opts.frames || opts.analyze {
        Some(Imaging::new()?)
    } else {
        None
    };
    if target.is_dir() {
        return run_folder(target, journal_dir, opts, imaging.as_ref(), &mut progress);
    }
    if !target.is_file() {
        return Err(format!("{} does not exist", target.display()));
    }
    // The clip's folder gives it context: overlaps and session neighbours.
    let dir = target.parent().unwrap_or(Path::new("."));
    let library = Library::scan(&[dir.to_owned()], journal_dir);
    let index = library
        .clips
        .iter()
        .position(|c| is_target(c, target))
        .or_else(|| library.position(&target.to_string_lossy()));
    let (library, index) = match index {
        Some(i) => (library, i),
        None => {
            // A sidecar whose video is gone (or a bundle's copy): digest
            // what it says.
            let clip = Clip::load(target).map_err(|e| {
                format!("{e}\n(no rec sidecar or session journal entry for this file)")
            })?;
            let mut alone = Library::default();
            alone.clips.push(clip);
            alone.journal = library.journal;
            (alone, 0)
        }
    };
    let clip = &library.clips[index];
    let out = opts
        .out
        .clone()
        .unwrap_or_else(|| dir.join(DIGEST_DIR).join(clip.stem()));
    let all = overlap::overlaps(&library.clips);
    let groups = overlap::groups(&library.clips);
    let group_of = overlap::group_of(&groups, library.clips.len());
    let ctx = Context {
        library: &library,
        overlaps: &all[index],
        group: group_of[index].as_deref(),
    };
    progress(&format!("{} → {}", clip.file, out.display()));
    let (done, _) = digest_clip(clip, &ctx, &out, opts, imaging.as_ref(), &mut progress)?;
    Ok(Report {
        done: vec![done],
        failed: Vec::new(),
        index: None,
    })
}

fn is_target(clip: &Clip, target: &Path) -> bool {
    let same = |p: &Option<PathBuf>| {
        p.as_deref().is_some_and(|p| {
            std::path::absolute(p)
                .ok()
                .map(|p| p.to_string_lossy().to_lowercase())
                == std::path::absolute(target)
                    .ok()
                    .map(|t| t.to_string_lossy().to_lowercase())
        })
    };
    same(&clip.video) || same(&clip.sidecar)
}

struct Context<'a> {
    library: &'a Library,
    overlaps: &'a [overlap::Overlap],
    group: Option<&'a str>,
}

fn run_folder(
    folder: &Path,
    journal_dir: &Path,
    opts: &Options,
    imaging: Option<&Imaging>,
    progress: &mut impl FnMut(&str),
) -> Result<Report, String> {
    let library = Library::scan(&[folder.to_owned()], journal_dir);
    for problem in &library.problems {
        progress(&format!("skipped: {problem}"));
    }
    if library.clips.is_empty() {
        return Err(format!(
            "no rec clips under {} (clips need a `.rec.json` sidecar or a session journal entry)",
            folder.display()
        ));
    }
    let out = opts.out.clone().unwrap_or_else(|| folder.join(DIGEST_DIR));
    std::fs::create_dir_all(&out).map_err(|e| format!("can't create {}: {e}", out.display()))?;
    let all = overlap::overlaps(&library.clips);
    let groups = overlap::groups(&library.clips);
    let group_of = overlap::group_of(&groups, library.clips.len());
    let names = bundle_names(&library.clips);
    let mut report = Report {
        done: Vec::new(),
        failed: Vec::new(),
        index: None,
    };
    let mut entries = Vec::new();
    let total = library.clips.len();
    for (i, clip) in library.clips.iter().enumerate() {
        progress(&format!("[{}/{total}] {}", i + 1, clip.file));
        let ctx = Context {
            library: &library,
            overlaps: &all[i],
            group: group_of[i].as_deref(),
        };
        let bundle = out.join(&names[i]);
        let mut entry = match digest_clip(clip, &ctx, &bundle, opts, imaging, progress) {
            Ok((done, analysed)) => {
                let entry = index_entry(&analysed, &library, &ctx, folder, Some(&names[i]));
                report.done.push(done);
                entry
            }
            Err(e) => {
                progress(&format!("  failed: {e}"));
                report.failed.push((clip.file.clone(), e.clone()));
                let mut entry = index_entry(clip, &library, &ctx, folder, None);
                entry["error"] = e.into();
                entry
            }
        };
        entry["overlaps"] = overlap::to_json(&library.clips, &all[i]);
        entries.push(entry);
    }
    let index = json!({
        "rec_index": 1,
        "generated_utc": format_utc(now()),
        "root": folder,
        "clips": entries,
        "groups": groups.iter().map(|g| json!({
            "id": g.id,
            "session": g.session,
            "game": library.clips[g.clips[0]].game.name,
            "from": round3(g.from),
            "to": round3(g.to),
            "clips": g.clips.iter().map(|&i| library.clips[i].id.as_str()).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "notes": "Times are seconds. session_range is the clip's span in its game session; two clips overlap when their ranges in the same session intersect. For an overlap, the other clip's time = this clip's time + offset.",
    });
    let path = out.join("index.json");
    write_json(&path, &index)?;
    report.index = Some(path);
    Ok(report)
}

/// One `index.json` entry.
fn index_entry(
    clip: &Clip,
    library: &Library,
    ctx: &Context,
    root: &Path,
    bundle: Option<&str>,
) -> Value {
    let entries = timeline::timeline(clip);
    let mut counts = serde_json::Map::new();
    for e in &entries {
        let n = counts.get(&e.kind).and_then(Value::as_u64).unwrap_or(0);
        counts.insert(e.kind.clone(), (n + 1).into());
    }
    let dir = clip.video.as_deref().and_then(Path::parent);
    let file = (clip.video.as_deref())
        .and_then(|v| relative(v, root))
        .unwrap_or_else(|| clip.file.clone());
    let thumb = (clip.analysis.as_ref())
        .and_then(|a| a["thumb"]["file"].as_str())
        .map(|t| {
            dir.and_then(|d| relative(&d.join(t), root))
                .unwrap_or_else(|| t.to_owned())
        });
    let position = clip
        .session
        .as_deref()
        .and_then(|s| library.journal.sessions.get(s))
        .and_then(|log| log.clip_position(&clip.id));
    json!({
        "id": clip.id,
        "file": file,
        "bundle": bundle,
        "source": match clip.source { Source::Sidecar => "sidecar", Source::Journal => "journal" },
        "game": clip.game.name,
        "start_utc": clip.start_utc.map(format_utc),
        "duration": round3(clip.duration),
        "starred": clip.starred,
        "session": clip.session,
        "session_range": clip.session_range().map(|(_, a, b)| json!([round3(a), round3(b)])),
        "session_position": position.map(|(n, of)| json!([n, of])),
        "events": counts,
        "overlap_group": ctx.group,
        "thumb": thumb,
    })
}

/// `path` relative to `root`, with forward slashes (valid on Windows and
/// unambiguous in JSON).
fn relative(path: &Path, root: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

/// Bundle folder names: the clip's stem, made unique with its id.
fn bundle_names(clips: &[Clip]) -> Vec<String> {
    let mut seen = HashSet::new();
    clips
        .iter()
        .map(|c| {
            let stem = c.stem();
            let name = if seen.insert(stem.to_lowercase()) {
                stem
            } else {
                format!("{stem}-{}", &c.id[..c.id.len().min(10)])
            };
            seen.insert(name.to_lowercase());
            name
        })
        .collect()
}

/// Writes one bundle; returns what was done and the clip as analysed.
fn digest_clip(
    clip: &Clip,
    ctx: &Context,
    out: &Path,
    opts: &Options,
    imaging: Option<&Imaging>,
    progress: &mut impl FnMut(&str),
) -> Result<(Done, Clip), String> {
    std::fs::create_dir_all(out).map_err(|e| format!("can't create {}: {e}", out.display()))?;
    let mut warnings = Vec::new();
    let mut clip = clip.clone();
    if opts.analyze
        && clip.analysis.is_none()
        && clip.video.is_some()
        && let Some(imaging) = imaging
    {
        match analysis::analyze(&clip, imaging) {
            Ok(analysed) => {
                if let Err(e) = analysis::save(&analysed) {
                    warnings.push(format!("analysis not saved: {e}"));
                }
                clip = analysed;
            }
            Err(e) => warnings.push(format!("no analysis: {e}")),
        }
    }
    let entries = timeline::timeline(&clip);
    let plan = plan_stills(&clip, &entries, opts.every);

    clear_bundle(out);
    let mut stills = Vec::new();
    let mut sheets = Vec::new();
    let mut no_stills = None;
    match (&clip.video, imaging) {
        (Some(video), Some(imaging)) if opts.frames => {
            match write_stills(video, &plan, out, opts, imaging) {
                Ok((s, tiles)) => {
                    sheets = write_sheets(&clip, &s, &tiles, out, opts, imaging)?;
                    stills = s;
                }
                Err(e) => {
                    warnings.push(format!("no stills: {e}"));
                    no_stills = Some(format!("the video could not be decoded ({e})"));
                }
            }
        }
        (None, _) => no_stills = Some("the video file is missing".into()),
        _ => no_stills = Some("`--no-frames`".into()),
    }

    let position = clip
        .session
        .as_deref()
        .and_then(|s| ctx.library.journal.sessions.get(s))
        .and_then(|log| log.clip_position(&clip.id));
    let overlaps = ctx
        .overlaps
        .iter()
        .map(|o| (ctx.library.clips[o.other].file.as_str(), o))
        .collect();
    let extras = Extras {
        position,
        overlaps,
        group: ctx.group,
        stills: &stills,
        still_width: opts.width,
        sheets: &sheets,
        no_stills,
    };
    let md = summary::render(&clip, &entries, &extras);
    std::fs::write(out.join("summary.md"), md)
        .map_err(|e| format!("can't write summary.md: {e}"))?;
    write_json(&out.join("clip.rec.json"), &clip.raw)?;
    for w in &warnings {
        progress(&format!("  {w}"));
    }
    Ok((
        Done {
            id: clip.id.clone(),
            file: clip.file.clone(),
            bundle: out.to_owned(),
            stills: stills.len(),
            sheets: sheets.len(),
            warnings,
        },
        clip,
    ))
}

/// A planned still: when, and why.
#[derive(Clone, Debug, PartialEq)]
pub struct Planned {
    pub t: f64,
    pub reasons: Vec<String>,
    /// Taken for an event rather than the clock.
    pub event: bool,
}

/// Stills at each event (the peak of a loud stretch), the save request and
/// every `every` seconds, merged when closer than 0.25 s.
pub fn plan_stills(clip: &Clip, entries: &[Entry], every: f64) -> Vec<Planned> {
    let last = (clip.duration - 0.02).max(0.0);
    let cuts = entries.iter().filter(|e| e.kind == "cut").count();
    let mut plan: Vec<Planned> = entries
        .iter()
        .filter(|e| e.kind != "cut" || cuts <= MAX_CUT_STILLS)
        .map(|e| Planned {
            t: e.at.clamp(0.0, last),
            reasons: vec![e.note.clone()],
            event: true,
        })
        .collect();
    if let Some(at) = clip.saved_at.filter(|&t| t >= 0.0 && t <= clip.duration) {
        plan.push(Planned {
            t: at.min(last),
            reasons: vec!["save requested".into()],
            event: true,
        });
    }
    if every > 0.0 && clip.duration > 0.0 {
        let label = format!("every {} s", trim_float(every));
        let mut t = 0.0;
        while t < clip.duration {
            plan.push(Planned {
                t,
                reasons: vec![label.clone()],
                event: false,
            });
            t = round3(t + every);
        }
        // The end of the clip, unless a periodic still is close to it.
        if clip.duration - (t - every) > every / 2.0 {
            plan.push(Planned {
                t: last,
                reasons: vec!["end of clip".into()],
                event: false,
            });
        }
    }
    // Events first at equal times, so a merged still keeps the event's time.
    plan.sort_by(|a, b| a.t.total_cmp(&b.t).then(b.event.cmp(&a.event)));
    let mut merged: Vec<Planned> = Vec::new();
    for p in plan {
        match merged.last_mut() {
            Some(m) if p.t - m.t < MERGE => {
                if p.event && !m.event {
                    m.t = p.t;
                    m.event = true;
                    m.reasons.insert(0, p.reasons[0].clone());
                } else {
                    m.reasons.extend(p.reasons);
                }
            }
            _ => merged.push(p),
        }
    }
    for m in &mut merged {
        m.reasons.dedup();
    }
    merged
}

fn trim_float(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// A contact sheet tile: the index of its still and a small copy of it.
type Tile = (usize, Frame);

/// Decodes and writes the stills; returns them and the sheet tiles (small
/// copies of the stills chosen for sheets).
fn write_stills(
    video: &Path,
    plan: &[Planned],
    out: &Path,
    opts: &Options,
    imaging: &Imaging,
) -> Result<(Vec<Still>, Vec<Tile>), String> {
    let frames_dir = out.join("frames");
    std::fs::create_dir_all(&frames_dir)
        .map_err(|e| format!("can't create {}: {e}", frames_dir.display()))?;
    let on_sheets = choose_for_sheets(plan, opts.sheets * opts.per_sheet);
    let mut decoder = Decoder::open(video, Some(opts.width))?;
    let mut stills: Vec<Still> = Vec::new();
    let mut tiles = Vec::new();
    for (i, p) in plan.iter().enumerate() {
        let frame = decoder.frame_at(p.t)?;
        let name = format!("{}.jpg", still_name(frame.t));
        // Two plans can land on one frame (a very short clip).
        if let Some(prev) = stills.last_mut().filter(|s| s.file.ends_with(&name)) {
            prev.reasons.extend(p.reasons.iter().cloned());
            continue;
        }
        imaging.save_jpeg(
            &imaging.scaled(&frame, opts.width)?,
            &frames_dir.join(&name),
        )?;
        if on_sheets.contains(&i) {
            tiles.push((
                stills.len(),
                imaging.shrink(&frame, opts.layout.tile_width)?,
            ));
        }
        stills.push(Still {
            t: round3(frame.t),
            file: format!("frames/{name}"),
            reasons: p.reasons.clone(),
        });
    }
    Ok((stills, tiles))
}

/// Which planned stills go on contact sheets: all if they fit, else every
/// event still and evenly spread clock stills.
fn choose_for_sheets(plan: &[Planned], capacity: usize) -> HashSet<usize> {
    if plan.len() <= capacity {
        return (0..plan.len()).collect();
    }
    let events: Vec<usize> = (0..plan.len()).filter(|&i| plan[i].event).collect();
    let clocks: Vec<usize> = (0..plan.len()).filter(|&i| !plan[i].event).collect();
    if events.len() >= capacity {
        return spread(&events, capacity).into_iter().collect();
    }
    let mut chosen: HashSet<usize> = events.iter().copied().collect();
    chosen.extend(spread(&clocks, capacity - events.len()));
    chosen
}

/// `n` items spread evenly over `items`, first and last included.
fn spread(items: &[usize], n: usize) -> Vec<usize> {
    match n {
        0 => Vec::new(),
        _ if n >= items.len() => items.to_vec(),
        1 => vec![items[0]],
        _ => (0..n)
            .map(|k| items[k * (items.len() - 1) / (n - 1)])
            .collect(),
    }
}

fn write_sheets(
    clip: &Clip,
    stills: &[Still],
    tiles: &[Tile],
    out: &Path,
    opts: &Options,
    imaging: &Imaging,
) -> Result<Vec<Sheet>, String> {
    let chunks: Vec<&[Tile]> = tiles.chunks(opts.per_sheet.max(1)).collect();
    let count = chunks.len();
    let mut sheets = Vec::new();
    for (n, chunk) in chunks.into_iter().enumerate() {
        let (from, to) = (stills[chunk[0].0].t, stills[chunk[chunk.len() - 1].0].t);
        let title = format!(
            "{} · {} · sheet {} of {count} · {}–{}",
            clip.file,
            clip.game.name,
            n + 1,
            clock(from),
            clock(to)
        );
        let labelled: Vec<(&Frame, String)> = chunk
            .iter()
            .map(|(i, f)| {
                (
                    f,
                    format!(
                        "{} · {}",
                        clock(stills[*i].t),
                        short(&stills[*i].reasons[0])
                    ),
                )
            })
            .collect();
        let file = format!("sheet-{}.jpg", n + 1);
        imaging.save_jpeg(
            &imaging.sheet(&title, &labelled, opts.layout)?,
            &out.join(&file),
        )?;
        sheets.push(Sheet {
            file,
            count: chunk.len(),
            from,
            to,
        });
    }
    Ok(sheets)
}

/// A reason short enough for a tile label.
fn short(reason: &str) -> String {
    let r = reason.split(" (").next().unwrap_or(reason);
    let r = r.split(", peak").next().unwrap_or(r);
    if r.chars().count() > 34 {
        format!("{}…", r.chars().take(33).collect::<String>())
    } else {
        r.to_owned()
    }
}

/// Removes stills and sheets from an earlier run of the same bundle.
fn clear_bundle(out: &Path) {
    let owned = |name: &str, prefix: &str| name.starts_with(prefix) && name.ends_with(".jpg");
    if let Ok(entries) = std::fs::read_dir(out.join("frames")) {
        for e in entries.flatten() {
            if owned(&e.file_name().to_string_lossy(), "t") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(out) {
        for e in entries.flatten() {
            if owned(&e.file_name().to_string_lossy(), "sheet-") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    let mut text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    text = analysis::compact_number_arrays(&text);
    text.push('\n');
    std::fs::write(path, text).map_err(|e| format!("can't write {}: {e}", path.display()))
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

    #[test]
    fn plans_event_and_clock_stills() {
        let clip = Clip::from_value(sample()).unwrap();
        let entries = timeline::timeline(&clip);
        let plan = plan_stills(&clip, &entries, 5.0);
        let times: Vec<f64> = plan.iter().map(|p| p.t).collect();
        // Clock stills at 0, 5, … 25; events at 3, 21.18, 24.9 (merged with
        // 25 and keeping its own time), the save at 27.5 and the last frame.
        assert_eq!(
            times,
            vec![0.0, 3.0, 5.0, 10.0, 15.0, 20.0, 21.18, 24.9, 27.5, 29.98],
            "{plan:?}"
        );
        let merged = &plan[7];
        assert!(merged.event);
        assert_eq!(merged.reasons, vec!["marker (hotkey)", "every 5 s"]);
        assert_eq!(plan[8].reasons, vec!["save requested"]);
    }

    #[test]
    fn adds_an_end_still_and_skips_many_cuts() {
        let mut s = sample();
        s["clip"]["duration"] = 13.0.into();
        s["clip"]["saved_at"] = Value::Null;
        let mut events = vec![];
        for i in 0..20 {
            events.push(json!({"t": 0.5 * f64::from(i), "kind": "cut", "src": "analysis:video", "conf": 0.9}));
        }
        s["events"] = events.into();
        s["analysis"] = json!({"v": 1});
        let clip = Clip::from_value(s).unwrap();
        let plan = plan_stills(&clip, &timeline::timeline(&clip), 5.0);
        let times: Vec<f64> = plan.iter().map(|p| p.t).collect();
        assert_eq!(times, vec![0.0, 5.0, 10.0, 12.98]);
        assert_eq!(plan[3].reasons, vec!["end of clip"]);
    }

    #[test]
    fn sheet_choice_keeps_events() {
        let plan: Vec<Planned> = (0..10)
            .map(|i| Planned {
                t: f64::from(i),
                reasons: vec![String::new()],
                event: i == 4,
            })
            .collect();
        assert_eq!(choose_for_sheets(&plan, 20).len(), 10);
        let mut chosen: Vec<usize> = choose_for_sheets(&plan, 3).into_iter().collect();
        chosen.sort_unstable();
        assert_eq!(chosen, vec![0, 4, 9]);
        assert_eq!(spread(&[1, 2, 3, 4, 5], 3), vec![1, 3, 5]);
    }

    #[test]
    fn names_and_labels() {
        let a = Clip::from_value(sample()).unwrap();
        let mut b = a.clone();
        b.id = "01OTHERCLIP000000000000000".into();
        assert_eq!(
            bundle_names(&[a.clone(), b]),
            vec![
                "Team Fortress 2_2026-10-17_21-04-33".to_owned(),
                "Team Fortress 2_2026-10-17_21-04-33-01OTHERCLI".to_owned()
            ]
        );
        assert_eq!(short("loud game audio, peak -12 dBFS"), "loud game audio");
        assert_eq!(short("marker (hotkey)"), "marker");
        assert_eq!(trim_float(5.0), "5");
        assert_eq!(trim_float(2.5), "2.5");
    }
}
