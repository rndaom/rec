//! A clip's timeline: the sidecar's events plus loud moments found in its
//! loudness curves (layer 2 "audio onsets and peaks", which needs no
//! decoding). Once `rec analyze` has run, the loud moments are stored in the
//! sidecar as events and are not derived again.

use serde_json::{Value, json};

use crate::clip::{Clip, Event, round3};
use crate::time::clock;

/// Most loud moments kept per track, the loudest first.
pub const MAX_LOUD_GAME: usize = 8;
pub const MAX_LOUD_VOICE: usize = 5;

/// dBFS of a loudness value (0 = -60 dBFS or quieter, 255 = 0 dBFS).
pub fn db(value: u8) -> f64 {
    f64::from(value) * 60.0 / 255.0 - 60.0
}

/// A stretch of a loudness curve that stands out from the rest of the clip.
#[derive(Clone, Debug, PartialEq)]
pub struct Loud {
    pub t: f64,
    pub dur: f64,
    pub peak_t: f64,
    pub peak_db: f64,
    pub conf: f64,
}

/// Loud stretches of a curve sampled at `hz`, at most `max` of them (the
/// loudest), in time order.
///
/// "Loud" is relative to the clip itself: well above its median level, and
/// never below -40 dBFS. A stretch is kept if its peak clears that threshold
/// by 6 dB or it lasts a second. A steady level (music, engine hum) gives
/// nothing.
pub fn loud_moments(values: &[u8], hz: u32, max: usize) -> Vec<Loud> {
    if values.len() < 3 || hz == 0 {
        return Vec::new();
    }
    let dbs: Vec<f64> = values.iter().map(|&v| db(v)).collect();
    let mut sorted = dbs.clone();
    sorted.sort_by(f64::total_cmp);
    let median = sorted[sorted.len() / 2];
    let p95 = sorted[(sorted.len() * 95 / 100).min(sorted.len() - 1)];
    if sorted[sorted.len() - 1] < -45.0 {
        return Vec::new();
    }
    let threshold = (median + 4.0).max(-40.0).max((median + 12.0).min(p95));
    let hz_f = f64::from(hz);
    let gap = (0.3 * hz_f).round() as usize;
    // Runs of samples at or above the threshold, joined across short gaps.
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (i, &d) in dbs.iter().enumerate() {
        if d < threshold {
            continue;
        }
        match runs.last_mut() {
            Some((_, end)) if i <= *end + gap => *end = i + 1,
            _ => runs.push((i, i + 1)),
        }
    }
    let mut moments: Vec<Loud> = runs
        .into_iter()
        .filter_map(|(start, end)| {
            let (peak_i, &peak) = (dbs[start..end].iter().enumerate())
                .max_by(|a, b| a.1.total_cmp(b.1))
                .map(|(i, d)| (start + i, d))?;
            let long = end - start >= hz as usize;
            (peak >= threshold + 6.0 || long).then(|| Loud {
                t: round3(start as f64 / hz_f),
                dur: round3((end - start) as f64 / hz_f),
                peak_t: round3((peak_i as f64 + 0.5) / hz_f),
                peak_db: (peak * 10.0).round() / 10.0,
                conf: ((0.4 + (peak - threshold) / 30.0).clamp(0.4, 0.9) * 100.0).round() / 100.0,
            })
        })
        .collect();
    moments.sort_by(|a, b| b.peak_db.total_cmp(&a.peak_db));
    moments.truncate(max);
    moments.sort_by(|a, b| a.t.total_cmp(&b.t));
    moments
}

/// Loud moments of the clip's game track (or mix) and its voice tracks, as
/// sidecar events (`kind: "loud"`, `src: "audio:<role>"`, conf < 1).
pub fn loud_events(clip: &Clip) -> Vec<Value> {
    let mut tracks = Vec::new();
    if let Some(game) = clip.game_track() {
        tracks.push((game, MAX_LOUD_GAME));
    }
    for role in ["mic", "voice"] {
        if let Some(track) = clip.audio.iter().find(|t| t.role == role) {
            tracks.push((track, MAX_LOUD_VOICE));
        }
    }
    let mut events: Vec<Value> = tracks
        .into_iter()
        .flat_map(|(track, max)| {
            loud_moments(&track.loudness, clip.hz, max)
                .into_iter()
                .map(move |m| {
                    json!({
                        "t": m.t,
                        "kind": "loud",
                        "src": format!("audio:{}", track.role),
                        "conf": m.conf,
                        "dur": m.dur,
                        "peak_t": m.peak_t,
                        "peak_db": m.peak_db,
                        "track": track.index,
                    })
                })
        })
        .collect();
    events.sort_by(|a, b| {
        a["t"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&b["t"].as_f64().unwrap_or(0.0))
    });
    events
}

/// One line of a timeline.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub t: f64,
    pub dur: Option<f64>,
    /// The best moment to look at: the peak of a loud stretch, else `t`.
    pub at: f64,
    pub kind: String,
    pub src: String,
    pub conf: f64,
    /// Plain language, e.g. "loud game audio, peak -12 dBFS".
    pub note: String,
    pub raw: Value,
}

impl Entry {
    fn from_event(e: &Event) -> Self {
        Self {
            t: e.t,
            dur: e.dur,
            at: e.raw["peak_t"].as_f64().unwrap_or(e.t),
            kind: e.kind.clone(),
            src: e.src.clone(),
            conf: e.conf,
            note: describe(e),
            raw: e.raw.clone(),
        }
    }

    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "t": self.t,
            "clock": clock(self.t),
            "kind": self.kind,
            "src": self.src,
            "conf": self.conf,
            "note": self.note,
        });
        if let Some(dur) = self.dur {
            v["dur"] = dur.into();
        }
        if self.at != self.t {
            v["look_at"] = self.at.into();
        }
        v
    }
}

/// Everything known to happen in the clip, in time order. Events from
/// before t = 0 or after the end are left out.
pub fn timeline(clip: &Clip) -> Vec<Entry> {
    let derived: Vec<Event> = if clip.analysis.is_none() && !has_loud(clip) {
        loud_events(clip)
            .iter()
            .filter_map(Event::from_value)
            .collect()
    } else {
        Vec::new()
    };
    let end = clip.duration.max(0.0) + 0.001;
    let mut entries: Vec<Entry> = (clip.events.iter())
        .chain(&derived)
        .filter(|e| (0.0..=end).contains(&e.t))
        .map(Entry::from_event)
        .collect();
    entries.sort_by(|a, b| a.t.total_cmp(&b.t));
    entries
}

fn has_loud(clip: &Clip) -> bool {
    clip.events
        .iter()
        .any(|e| e.kind == "loud" && e.src.starts_with("audio:"))
}

/// Plain language for an event.
pub fn describe(e: &Event) -> String {
    let raw = &e.raw;
    let mut note = match e.kind.as_str() {
        "marker" => match e.src.as_str() {
            "hotkey" => "marker (hotkey)".to_owned(),
            "cli" => "marker (rec mark)".to_owned(),
            src => format!("marker ({src})"),
        },
        "loud" => {
            let what = match e.src.strip_prefix("audio:") {
                Some("mic") => "loud microphone (the player speaking or reacting)",
                Some("voice") => "loud voice chat",
                Some("mix") => "loud audio",
                _ => "loud game audio",
            };
            match raw["peak_db"].as_f64() {
                Some(peak) => format!("{what}, peak {peak:.0} dBFS"),
                None => what.to_owned(),
            }
        }
        "cut" => "scene cut".to_owned(),
        "menu" => "menu or static screen".to_owned(),
        kind => kind.replace('_', " "),
    };
    // Who and what, for events from game sources.
    let actor = raw["actor"].as_str();
    let target = raw["target"].as_str();
    match (actor, target) {
        (Some(a), Some(t)) => note.push_str(&format!(": {a} → {t}")),
        (Some(a), None) => note.push_str(&format!(": {a}")),
        (None, Some(t)) => note.push_str(&format!(": {t}")),
        (None, None) => {}
    }
    if let Some(data) = raw["data"].as_object() {
        let parts: Vec<String> = data
            .iter()
            .map(|(k, v)| match v {
                Value::Bool(true) => k.clone(),
                Value::String(s) => s.clone(),
                other => format!("{k} {other}"),
            })
            .collect();
        if !parts.is_empty() {
            note.push_str(&format!(" ({})", parts.join(", ")));
        }
    }
    if e.conf < 1.0 && !matches!(e.kind.as_str(), "loud") {
        note.push_str(&format!(" [conf {:.2}]", e.conf));
    }
    note
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::tests::sample;

    /// dBFS to a loudness value.
    fn v(db: f64) -> u8 {
        ((db + 60.0) * 255.0 / 60.0).round() as u8
    }

    #[test]
    fn finds_shots_over_quiet_background() {
        let mut curve = vec![v(-50.0); 100];
        curve[20] = v(-12.0); // a shot
        curve[21] = v(-20.0);
        curve[60..75].fill(v(-25.0)); // a 1.5 s fight
        curve[90] = v(-36.0); // a footstep: too quiet to count
        let m = loud_moments(&curve, 10, 8);
        assert_eq!(m.len(), 2, "{m:?}");
        assert_eq!((m[0].t, m[0].dur, m[0].peak_t), (2.0, 0.2, 2.05));
        assert!((m[0].peak_db + 12.0).abs() < 0.2);
        assert_eq!((m[1].t, m[1].dur), (6.0, 1.5));
        assert!(m[0].conf > m[1].conf);
    }

    #[test]
    fn joins_close_peaks_and_keeps_the_loudest() {
        let mut curve = vec![v(-50.0); 200];
        for (i, at) in [10usize, 12, 50, 90, 130, 170].into_iter().enumerate() {
            curve[at] = v(-30.0 + i as f64);
        }
        let m = loud_moments(&curve, 10, 3);
        // 10 and 12 are one stretch; the three loudest stretches remain.
        assert_eq!(
            m.iter().map(|m| m.t).collect::<Vec<_>>(),
            vec![9.0, 13.0, 17.0]
        );
        assert_eq!(loud_moments(&curve, 10, 10)[0].dur, 0.3);
    }

    #[test]
    fn steady_or_silent_audio_has_no_loud_moments() {
        assert!(loud_moments(&[v(-20.0); 100], 10, 8).is_empty());
        assert!(loud_moments(&[0; 100], 10, 8).is_empty());
        assert!(loud_moments(&[255, 255], 10, 8).is_empty());
    }

    #[test]
    fn timeline_merges_events_and_derived_loudness() {
        let mut s = sample();
        let mut game = vec![v(-50.0); 300];
        game[100] = v(-10.0);
        s["audio"][1]["loudness"] = json!(game);
        s["events"]
            .as_array_mut()
            .unwrap()
            .push(json!({"t": 40.0, "kind": "marker", "src": "hotkey", "conf": 1}));
        let clip = Clip::from_value(s.clone()).unwrap();
        let entries = timeline(&clip);
        let kinds: Vec<(&str, f64)> = entries.iter().map(|e| (e.kind.as_str(), e.t)).collect();
        // The marker past the end (40 > 30 s) is dropped.
        assert_eq!(
            kinds,
            vec![
                ("tf2:uber", 3.0),
                ("loud", 10.0),
                ("marker", 21.18),
                ("marker", 24.9)
            ]
        );
        assert_eq!(entries[1].src, "audio:game");
        assert_eq!(entries[1].at, 10.05);
        assert_eq!(entries[1].note, "loud game audio, peak -10 dBFS");
        assert_eq!(entries[2].note, "marker (rec mark)");
        assert_eq!(entries[1].to_json()["look_at"], 10.05);

        // Once analysed, loud moments come only from the sidecar.
        s["analysis"] = json!({"v": 1});
        let clip = Clip::from_value(s).unwrap();
        assert!(timeline(&clip).iter().all(|e| e.kind != "loud"));
    }

    #[test]
    fn describes_game_source_events() {
        let e = Event::from_value(&json!({
            "t": 21.18, "kind": "kill", "src": "hud:tf2", "conf": 0.8,
            "actor": "RNDAOM", "target": "xX_spy_Xx",
            "data": { "weapon": "sniperrifle", "crit": true }
        }))
        .unwrap();
        assert_eq!(
            describe(&e),
            "kill: RNDAOM → xX_spy_Xx (sniperrifle, crit) [conf 0.80]"
        );
    }
}
