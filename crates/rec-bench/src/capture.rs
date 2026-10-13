//! Reading PresentMon CSV files.
//!
//! PresentMon has changed its column names between versions, so columns are
//! found by name (case-insensitively) rather than by position:
//!
//! | what       | PresentMon 2.x            | PresentMon 1.x        |
//! |------------|---------------------------|-----------------------|
//! | frame time | `FrameTime` or `MsBetweenPresents` | `msBetweenPresents` |
//! | timestamp  | `CPUStartQPC` (`--qpc_time`) or `CPUStartTime` | `TimeInSeconds` |
//!
//! Only `CPUStartQPC` is on the QPC clock and can be lined up with the times
//! at which the bench asked `recd` to save a clip. The other timestamps are
//! relative to the start of the capture, so save-spike analysis is
//! unavailable for them.

use rec_core::time::{Hns, qpc_to_hns};

/// A cleaned-up capture of one application's frames, in file order.
#[derive(Clone, Debug, PartialEq)]
pub struct Capture {
    pub application: Option<String>,
    pub swap_chain: Option<String>,
    /// Milliseconds between presents, one entry per frame.
    pub frame_ms: Vec<f64>,
    /// Present time of each frame on the QPC clock, parallel to `frame_ms`.
    /// `None` when the file has no `CPUStartQPC` column.
    pub timestamps: Option<Vec<Hns>>,
    /// Rows dropped because a value was missing (`NA`) or not a number.
    pub skipped_rows: usize,
    /// Things worth telling the user (several apps, several swap chains,
    /// no QPC timestamps).
    pub notes: Vec<String>,
}

/// Splits one CSV line into fields. Handles double-quoted fields (with `""`
/// as an escaped quote) so a stray quoted comma cannot shift the columns; the
/// columns we read never contain one, but a quoted `Application` could.
pub fn split_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut field)),
            _ => field.push(c),
        }
    }
    fields.push(field);
    fields
}

#[derive(Clone, Copy, PartialEq)]
enum TimeKind {
    Qpc,
    /// Present but relative to capture start: not alignable.
    Relative,
}

struct Row {
    application: Option<String>,
    swap_chain: Option<String>,
    frame_ms: f64,
    qpc: Option<i64>,
}

/// Whether the `Application` column value `app` is the process the user asked
/// for. Case-insensitive, and `game` matches `Game.exe`.
fn app_matches(app: &str, wanted: &str) -> bool {
    fn stem(s: &str) -> &str {
        let s = s.trim();
        match s.len().checked_sub(4) {
            Some(i) if s.is_char_boundary(i) && s[i..].eq_ignore_ascii_case(".exe") => &s[..i],
            _ => s,
        }
    }
    stem(app).eq_ignore_ascii_case(stem(wanted))
}

/// Returns the key with the most occurrences; ties go to the first seen.
fn most_common<'a>(keys: impl Iterator<Item = &'a str>) -> Option<(String, usize)> {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for key in keys {
        match counts.iter_mut().find(|(k, _)| *k == key) {
            Some((_, n)) => *n += 1,
            None => counts.push((key, 1)),
        }
    }
    let distinct = counts.len();
    counts
        .into_iter()
        .fold(None, |best: Option<(&str, usize)>, c| match best {
            Some(b) if b.1 >= c.1 => Some(b),
            _ => Some(c),
        })
        .map(|(k, _)| (k.to_string(), distinct))
}

/// Parses PresentMon CSV text.
///
/// `qpc_frequency` converts `CPUStartQPC` counter values to [`Hns`]. `wanted`
/// restricts the capture to one application; without it the application with
/// the most rows is used. If that application presented through several swap
/// chains (overlays and launchers do), the one with the most rows is used.
pub fn parse(text: &str, qpc_frequency: i64, wanted: Option<&str>) -> Result<Capture, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let header = lines.next().ok_or("the file is empty")?;
    let names: Vec<String> = split_line(header)
        .iter()
        .map(|n| n.trim().to_ascii_lowercase())
        .collect();
    let find = |candidates: &[&str]| {
        candidates
            .iter()
            .find_map(|c| names.iter().position(|n| n == c))
    };

    // `MsBetweenPresents` is spelled `msBetweenPresents` in 1.x; names are
    // lower-cased above, so one entry covers both.
    let frame_col = find(&["msbetweenpresents", "frametime"]).ok_or_else(|| {
        format!(
            "no frame-time column (MsBetweenPresents / FrameTime); columns are: {}",
            split_line(header).join(", ")
        )
    })?;
    let (time_col, time_kind) = match (
        find(&["cpustartqpc"]),
        find(&["cpustarttime", "timeinseconds"]),
    ) {
        (Some(c), _) => (Some(c), Some(TimeKind::Qpc)),
        (None, Some(c)) => (Some(c), Some(TimeKind::Relative)),
        (None, None) => (None, None),
    };
    let app_col = find(&["application", "processname"]);
    let chain_col = find(&["swapchainaddress"]);

    let mut rows = Vec::new();
    let mut skipped_rows = 0;
    for line in lines {
        let fields = split_line(line);
        let get = |col: usize| fields.get(col).map(|s| s.trim());
        let frame_ms = get(frame_col).and_then(|s| s.parse::<f64>().ok());
        let Some(frame_ms) = frame_ms.filter(|ms| ms.is_finite() && *ms > 0.0) else {
            skipped_rows += 1;
            continue;
        };
        let qpc = match (time_col, time_kind) {
            (Some(col), Some(TimeKind::Qpc)) => {
                match get(col).and_then(|s| s.parse::<i64>().ok()) {
                    Some(v) => Some(v),
                    None => {
                        skipped_rows += 1;
                        continue;
                    }
                }
            }
            (Some(col), Some(TimeKind::Relative)) => {
                if get(col).and_then(|s| s.parse::<f64>().ok()).is_none() {
                    skipped_rows += 1;
                    continue;
                }
                None
            }
            _ => None,
        };
        rows.push(Row {
            application: app_col.and_then(get).map(String::from),
            swap_chain: chain_col.and_then(get).map(String::from),
            frame_ms,
            qpc,
        });
    }
    if rows.is_empty() {
        return Err(format!(
            "no usable frames in the file ({skipped_rows} rows skipped for missing or invalid values)"
        ));
    }

    let mut notes = Vec::new();

    // Pick the application.
    let application = if app_col.is_some() {
        let apps: Vec<&str> = rows
            .iter()
            .map(|r| r.application.as_deref().unwrap_or(""))
            .collect();
        match wanted {
            Some(wanted) => {
                let found = apps.iter().find(|a| app_matches(a, wanted));
                let Some(&found) = found else {
                    let mut distinct: Vec<&str> = Vec::new();
                    for a in &apps {
                        if !distinct.contains(a) {
                            distinct.push(a);
                        }
                    }
                    return Err(format!(
                        "no frames for `{wanted}` in the file; applications present: {}",
                        distinct.join(", ")
                    ));
                };
                // Different casings of one process count as the same app.
                Some(found.to_string())
            }
            None => {
                let (app, distinct) = most_common(apps.iter().copied()).expect("rows is not empty");
                if distinct > 1 {
                    notes.push(format!(
                        "{distinct} applications in the file; using `{app}` (most frames). \
                         Pass --process to choose another."
                    ));
                }
                Some(app)
            }
        }
    } else {
        if wanted.is_some() {
            notes.push("the file has no Application column; not filtering by process".into());
        }
        None
    };
    if let Some(app) = &application {
        rows.retain(|r| {
            r.application
                .as_deref()
                .is_some_and(|a| app_matches(a, app))
        });
    }

    // Pick the swap chain.
    let swap_chain = if chain_col.is_some() {
        let (chain, distinct) =
            most_common(rows.iter().map(|r| r.swap_chain.as_deref().unwrap_or("")))
                .expect("rows is not empty");
        if distinct > 1 {
            notes.push(format!(
                "{distinct} swap chains; using {chain} (most frames)"
            ));
        }
        rows.retain(|r| r.swap_chain.as_deref().unwrap_or("") == chain);
        Some(chain)
    } else {
        None
    };

    let timestamps = (time_kind == Some(TimeKind::Qpc)).then(|| {
        rows.iter()
            .map(|r| qpc_to_hns(r.qpc.unwrap_or_default(), qpc_frequency))
            .collect()
    });
    if timestamps.is_none() {
        notes.push(match time_kind {
            Some(_) => "timestamps are relative (no CPUStartQPC; run PresentMon with --qpc_time): \
                 save-spike analysis is unavailable"
                .to_string(),
            None => "no timestamp column: save-spike analysis is unavailable".to_string(),
        });
    }
    if skipped_rows > 0 {
        notes.push(format!(
            "{skipped_rows} rows skipped for missing (NA) or invalid values"
        ));
    }

    Ok(Capture {
        application,
        swap_chain,
        frame_ms: rows.iter().map(|r| r.frame_ms).collect(),
        timestamps,
        skipped_rows,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 10 MHz, the usual QPC frequency, so one tick is one `Hns`.
    const FREQ: i64 = 10_000_000;

    #[test]
    fn splits_plain_and_quoted_fields() {
        assert_eq!(split_line("a,b,,c"), ["a", "b", "", "c"]);
        assert_eq!(split_line(r#""a,b",c"#), ["a,b", "c"]);
        assert_eq!(split_line(r#""say ""hi""",x"#), [r#"say "hi""#, "x"]);
        assert_eq!(split_line(""), [""]);
    }

    #[test]
    fn v2_ms_between_presents_with_qpc() {
        let csv = "Application,ProcessID,SwapChainAddress,MsBetweenPresents,CPUStartQPC\n\
                   game.exe,10,0xA,16.5,1000000\n\
                   game.exe,10,0xA,17.5,1165000\n\
                   game.exe,10,0xA,16.0,1340000\n";
        let c = parse(csv, FREQ, Some("game.exe")).unwrap();
        assert_eq!(c.frame_ms, [16.5, 17.5, 16.0]);
        assert_eq!(
            c.timestamps.as_deref(),
            Some(&[1_000_000, 1_165_000, 1_340_000][..])
        );
        assert_eq!(c.application.as_deref(), Some("game.exe"));
        assert_eq!(c.swap_chain.as_deref(), Some("0xA"));
        assert!(c.notes.is_empty(), "{:?}", c.notes);
    }

    #[test]
    fn qpc_counter_is_converted_with_the_frequency() {
        // A 3 MHz counter: 3_000_000 ticks is one second = 10_000_000 Hns.
        let csv = "Application,FrameTime,CPUStartQPC\ngame.exe,10,3000000\n";
        let c = parse(csv, 3_000_000, None).unwrap();
        assert_eq!(c.timestamps.unwrap(), [10_000_000]);
    }

    #[test]
    fn v2_frame_time_column() {
        let csv = "Application,FrameTime,CPUStartQPC\ngame.exe,8.0,100\ngame.exe,9.0,200\n";
        let c = parse(csv, FREQ, None).unwrap();
        assert_eq!(c.frame_ms, [8.0, 9.0]);
        assert!(c.timestamps.is_some());
    }

    #[test]
    fn ms_between_presents_wins_over_frame_time() {
        let csv = "Application,FrameTime,MsBetweenPresents\ngame.exe,1.0,2.0\n";
        assert_eq!(parse(csv, FREQ, None).unwrap().frame_ms, [2.0]);
    }

    #[test]
    fn v1_columns_have_relative_time_only() {
        let csv = "\u{feff}Application,ProcessID,SwapChainAddress,Runtime,msBetweenPresents,TimeInSeconds\r\n\
                   Game.exe,10,0xA,DXGI,16.7,0.1\r\n\
                   Game.exe,10,0xA,DXGI,16.6,0.2\r\n";
        let c = parse(csv, FREQ, Some("game")).unwrap();
        assert_eq!(c.frame_ms, [16.7, 16.6]);
        assert!(c.timestamps.is_none());
        assert!(c.notes.iter().any(|n| n.contains("relative")));
    }

    #[test]
    fn cpu_start_time_without_qpc_is_relative() {
        let csv = "Application,FrameTime,CPUStartTime\ngame.exe,10,1.5\n";
        let c = parse(csv, FREQ, None).unwrap();
        assert!(c.timestamps.is_none());
        assert!(c.notes.iter().any(|n| n.contains("--qpc_time")));
    }

    #[test]
    fn column_names_are_case_insensitive() {
        let csv = "APPLICATION,msbetweenpresents,cpustartqpc\ngame.exe,5,10\n";
        let c = parse(csv, FREQ, None).unwrap();
        assert_eq!(c.frame_ms, [5.0]);
        assert_eq!(c.timestamps.unwrap(), [10]);
    }

    #[test]
    fn na_values_are_skipped() {
        let csv = "Application,FrameTime,CPUStartQPC\n\
                   game.exe,NA,100\n\
                   game.exe,10,200\n\
                   game.exe,10,NA\n\
                   game.exe,0,300\n\
                   game.exe,-1,400\n\
                   game.exe,12,500\n\
                   game.exe\n";
        let c = parse(csv, FREQ, None).unwrap();
        assert_eq!(c.frame_ms, [10.0, 12.0]);
        assert_eq!(c.timestamps.unwrap(), [200, 500]);
        assert_eq!(c.skipped_rows, 5);
        assert!(c.notes.iter().any(|n| n.contains("5 rows skipped")));
    }

    #[test]
    fn filters_to_the_requested_application() {
        let csv = "Application,FrameTime,CPUStartQPC\n\
                   launcher.exe,50,1\n\
                   game.exe,10,2\n\
                   launcher.exe,50,3\n\
                   launcher.exe,50,4\n\
                   game.exe,11,5\n";
        let c = parse(csv, FREQ, Some("GAME.EXE")).unwrap();
        assert_eq!(c.frame_ms, [10.0, 11.0]);
        assert_eq!(c.timestamps.unwrap(), [2, 5]);
    }

    #[test]
    fn without_a_filter_the_busiest_application_wins() {
        let csv = "Application,FrameTime\na.exe,1\nb.exe,2\nb.exe,3\n";
        let c = parse(csv, FREQ, None).unwrap();
        assert_eq!(c.application.as_deref(), Some("b.exe"));
        assert_eq!(c.frame_ms, [2.0, 3.0]);
        assert!(c.notes.iter().any(|n| n.contains("2 applications")));
    }

    #[test]
    fn unknown_application_lists_what_is_present() {
        let csv = "Application,FrameTime\na.exe,1\nb.exe,2\n";
        let err = parse(csv, FREQ, Some("c.exe")).unwrap_err();
        assert!(
            err.contains("c.exe") && err.contains("a.exe") && err.contains("b.exe"),
            "{err}"
        );
    }

    #[test]
    fn the_busiest_swap_chain_wins() {
        let csv = "Application,SwapChainAddress,FrameTime,CPUStartQPC\n\
                   game.exe,0x1,100,1\n\
                   game.exe,0x2,10,2\n\
                   game.exe,0x2,11,3\n\
                   game.exe,0x1,100,4\n\
                   game.exe,0x2,12,5\n";
        let c = parse(csv, FREQ, Some("game.exe")).unwrap();
        assert_eq!(c.swap_chain.as_deref(), Some("0x2"));
        assert_eq!(c.frame_ms, [10.0, 11.0, 12.0]);
        assert_eq!(c.timestamps.unwrap(), [2, 3, 5]);
        assert!(c.notes.iter().any(|n| n.contains("2 swap chains")));
    }

    #[test]
    fn quoted_application_with_a_comma_does_not_shift_columns() {
        let csv = "Application,FrameTime\n\"my,game.exe\",7\n";
        let c = parse(csv, FREQ, None).unwrap();
        assert_eq!(c.application.as_deref(), Some("my,game.exe"));
        assert_eq!(c.frame_ms, [7.0]);
    }

    #[test]
    fn rejects_files_without_usable_data() {
        assert!(parse("", FREQ, None).is_err());
        let err = parse("Application,Foo\ngame.exe,1\n", FREQ, None).unwrap_err();
        assert!(err.contains("no frame-time column"), "{err}");
        let err = parse("Application,FrameTime\ngame.exe,NA\n", FREQ, None).unwrap_err();
        assert!(err.contains("no usable frames"), "{err}");
    }

    #[test]
    fn works_without_an_application_column() {
        let c = parse("FrameTime\n5\n6\n", FREQ, Some("game.exe")).unwrap();
        assert_eq!(c.frame_ms, [5.0, 6.0]);
        assert!(c.application.is_none());
        assert!(c.notes.iter().any(|n| n.contains("no Application column")));
    }
}
