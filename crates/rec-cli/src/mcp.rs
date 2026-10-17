//! `rec mcp`: the clip library as a Model Context Protocol server on stdin
//! and stdout (JSON-RPC 2.0, one message per line), so an agent can query
//! clips instead of reading files. Local only: no sockets, nothing leaves
//! the machine. Tools: `list_clips`, `clip`, `timeline`, `frames`,
//! `audio_curve`, `overlaps` and `analyze` (see `docs/ai-clips.md`).

use std::io::{BufRead, Write};
use std::path::PathBuf;

use rec_library::clip::round3;
use rec_library::decode::Decoder;
use rec_library::image::Imaging;
use rec_library::time::{clock, format_local, format_utc, parse_utc};
use rec_library::timeline::{db, timeline};
use rec_library::{Library, analysis, b64, overlap, sys};
use serde_json::{Map, Value, json};

/// Protocol versions this server speaks, newest first.
const VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const MAX_FRAMES: usize = 8;
const DEFAULT_FRAME_WIDTH: u32 = 768;

const INSTRUCTIONS: &str = "\
rec's saved game clips. Every clip has an id (a ULID) and a sidecar with the game, \
wall-clock time, its span in a game session, audio track roles, markers the player \
added, loudness per audio track at 10 Hz and, once analysed, brightness, motion and \
scene cuts. All times are seconds from the clip's first frame unless named otherwise. \
Start with list_clips, read timeline for candidates, then ask frames for just the \
moments you need. Two clips overlap when their session ranges intersect (overlaps).";

pub fn serve(roots: Vec<PathBuf>, journal_dir: PathBuf) -> Result<(), String> {
    sys::init()?;
    let mut server = Server {
        library: Library::scan(&roots, &journal_dir),
        journal_dir,
        imaging: None,
        decoder: None,
    };
    eprintln!(
        "rec mcp: {} clips under {}",
        server.library.clips.len(),
        (roots.iter().map(|r| r.display().to_string()))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| format!("stdin: {e}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(Value::Array(batch)) => {
                let replies: Vec<Value> = batch.iter().filter_map(|m| server.handle(m)).collect();
                (!replies.is_empty()).then(|| Value::Array(replies))
            }
            Ok(message) => server.handle(&message),
            Err(e) => Some(error(Value::Null, -32700, &format!("parse error: {e}"))),
        };
        if let Some(reply) = reply {
            writeln!(stdout, "{reply}").map_err(|e| format!("stdout: {e}"))?;
            stdout.flush().map_err(|e| format!("stdout: {e}"))?;
        }
    }
    Ok(())
}

struct Server {
    library: Library,
    journal_dir: PathBuf,
    imaging: Option<Imaging>,
    /// The last clip decoded, kept open for follow-up requests.
    decoder: Option<(PathBuf, u32, Decoder)>,
}

impl Server {
    /// The reply to one message, if it needs one.
    fn handle(&mut self, message: &Value) -> Option<Value> {
        let id = message.get("id").cloned();
        let method = message["method"].as_str();
        let Some(id) = id else {
            // A notification (`notifications/initialized`, cancellations): no reply.
            return None;
        };
        let Some(method) = method else {
            // A response to something we never send, or garbage.
            return message
                .get("result")
                .or(message.get("error"))
                .is_none()
                .then(|| error(id, -32600, "invalid request"));
        };
        let params = &message["params"];
        Some(match method {
            "initialize" => {
                let asked = params["protocolVersion"].as_str().unwrap_or_default();
                let version = VERSIONS
                    .iter()
                    .find(|v| **v == asked)
                    .unwrap_or(&VERSIONS[0]);
                result(
                    id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": { "tools": { "listChanged": false } },
                        "serverInfo": { "name": "rec", "version": env!("CARGO_PKG_VERSION") },
                        "instructions": INSTRUCTIONS,
                    }),
                )
            }
            "ping" => result(id, json!({})),
            "tools/list" => result(id, json!({ "tools": tools() })),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default();
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                if !tools().iter().any(|t| t["name"] == name) {
                    return Some(error(id, -32602, &format!("unknown tool `{name}`")));
                }
                let content = match self.call(name, &args) {
                    Ok(content) => json!({ "content": content }),
                    Err(e) => json!({ "content": [text(&e)], "isError": true }),
                };
                result(id, content)
            }
            other => error(id, -32601, &format!("unknown method `{other}`")),
        })
    }

    fn call(&mut self, name: &str, args: &Value) -> Result<Vec<Value>, String> {
        self.library.refresh(&self.journal_dir);
        match name {
            "list_clips" => Ok(vec![json_text(&self.list_clips(args)?)]),
            "clip" => Ok(vec![json_text(&self.clip(args)?)]),
            "timeline" => Ok(vec![json_text(&self.timeline(args)?)]),
            "frames" => self.frames(args),
            "audio_curve" => Ok(vec![json_text(&self.audio_curve(args)?)]),
            "overlaps" => Ok(vec![json_text(&self.overlaps(args)?)]),
            "analyze" => Ok(vec![json_text(&self.analyze(args)?)]),
            _ => Err(format!("unknown tool `{name}`")),
        }
    }

    fn find(&self, args: &Value) -> Result<usize, String> {
        let key = args["id"].as_str().ok_or("`id` is required")?;
        self.library.position(key).ok_or_else(|| {
            format!(
                "no clip `{key}`; list_clips gives the ids ({} clips known)",
                self.library.clips.len()
            )
        })
    }

    fn list_clips(&self, args: &Value) -> Result<Value, String> {
        let game = args["game"].as_str().map(str::to_lowercase);
        let since = time_arg(args, "since")?;
        let until = time_arg(args, "until")?;
        let has_event = args["has_event"].as_str();
        let limit = args["limit"].as_u64().unwrap_or(50).clamp(1, 500) as usize;
        let oldest_first = args["order"].as_str() == Some("oldest");
        let groups = overlap::groups(&self.library.clips);
        let group_of = overlap::group_of(&groups, self.library.clips.len());
        let mut matching: Vec<Value> = Vec::new();
        for (i, clip) in self.library.clips.iter().enumerate() {
            if let Some(g) = &game
                && !clip.game.name.to_lowercase().contains(g.as_str())
                && !clip.game.exe.to_lowercase().contains(g.as_str())
            {
                continue;
            }
            let start = clip.start_utc;
            if since.is_some_and(|s| start.is_none_or(|t| t < s))
                || until.is_some_and(|u| start.is_none_or(|t| t >= u))
            {
                continue;
            }
            let entries = timeline(clip);
            if let Some(kind) = has_event
                && !entries.iter().any(|e| e.kind == kind)
            {
                continue;
            }
            let mut counts = Map::new();
            for e in &entries {
                let n = counts.get(&e.kind).and_then(Value::as_u64).unwrap_or(0);
                counts.insert(e.kind.clone(), (n + 1).into());
            }
            matching.push(json!({
                "id": clip.id,
                "file": clip.file,
                "path": clip.video,
                "game": clip.game.name,
                "start_utc": start.map(format_utc),
                "local_time": start.zip(clip.utc_offset_min).map(|(t, off)| format_local(t, off)),
                "duration": round3(clip.duration),
                "session": clip.session,
                "session_range": clip.session_range().map(|(_, a, b)| json!([round3(a), round3(b)])),
                "events": counts,
                "overlap_group": group_of[i],
                "analysed": clip.analysis.is_some(),
            }));
        }
        let total = matching.len();
        if !oldest_first {
            matching.reverse();
        }
        matching.truncate(limit);
        Ok(json!({
            "total": total,
            "shown": matching.len(),
            "order": if oldest_first { "oldest first" } else { "newest first" },
            "clips": matching,
        }))
    }

    fn clip(&self, args: &Value) -> Result<Value, String> {
        let i = self.find(args)?;
        let clip = &self.library.clips[i];
        let mut v = clip.raw.clone();
        // Curves are long; audio_curve serves them.
        if let Some(tracks) = v["audio"].as_array_mut() {
            for (n, t) in tracks.iter_mut().enumerate() {
                let curve = clip
                    .audio
                    .get(n)
                    .map(|t| t.loudness.as_slice())
                    .unwrap_or_default();
                match curve.iter().copied().max() {
                    None => {}
                    Some(0) => {
                        t["loudness"] = json!({ "omitted": "silent or muted throughout" });
                    }
                    Some(peak) => {
                        let at = curve.iter().position(|&x| x == peak).unwrap_or(0);
                        let mut sorted = curve.to_vec();
                        sorted.sort_unstable();
                        t["loudness"] = json!({
                            "omitted": "use audio_curve",
                            "median_dbfs": db(sorted[sorted.len() / 2]).round(),
                            "peak_dbfs": db(peak).round(),
                            "peak_t": round3((at as f64 + 0.5) / f64::from(clip.hz)),
                        });
                    }
                }
            }
        }
        v["path"] = json!(clip.video);
        v["sidecar_path"] = json!(clip.sidecar);
        let all = overlap::overlaps(&self.library.clips);
        v["overlaps"] = overlap::to_json(&self.library.clips, &all[i]);
        if let Some((n, of)) = clip
            .session
            .as_deref()
            .and_then(|s| self.library.journal.sessions.get(s))
            .and_then(|log| log.clip_position(&clip.id))
        {
            v["session_position"] = json!({ "clip": n, "of": of });
        }
        Ok(v)
    }

    fn timeline(&self, args: &Value) -> Result<Value, String> {
        let clip = &self.library.clips[self.find(args)?];
        let entries = timeline(clip);
        let mut v = json!({
            "id": clip.id,
            "file": clip.file,
            "game": clip.game.name,
            "duration": round3(clip.duration),
            "saved_at": clip.saved_at,
            "entries": entries.iter().map(|e| e.to_json()).collect::<Vec<_>>(),
            "analysed": clip.analysis.is_some(),
        });
        match &clip.analysis {
            Some(a) => v["picture"] = analysis::describe(a).into(),
            None => {
                v["hint"] = "Not analysed yet: loud moments come from the loudness curves; call analyze for scene cuts, brightness and motion.".into();
            }
        }
        Ok(v)
    }

    fn frames(&mut self, args: &Value) -> Result<Vec<Value>, String> {
        let i = self.find(args)?;
        let clip = &self.library.clips[i];
        let video = clip
            .video
            .clone()
            .ok_or("the clip's video file is missing")?;
        let times: Vec<f64> = args["times"]
            .as_array()
            .ok_or("`times` must be an array of clip times in seconds")?
            .iter()
            .map(|t| t.as_f64().ok_or("`times` must hold numbers"))
            .collect::<Result<_, _>>()?;
        if times.is_empty() {
            return Err("`times` is empty".into());
        }
        if times.len() > MAX_FRAMES {
            return Err(format!("at most {MAX_FRAMES} frames per call"));
        }
        let width = args["width"]
            .as_u64()
            .map_or(DEFAULT_FRAME_WIDTH, |w| w.clamp(64, 1920) as u32);
        let duration = clip.duration;
        if self.imaging.is_none() {
            self.imaging = Some(Imaging::new()?);
        }
        let reuse = matches!(&self.decoder, Some((p, w, _)) if *p == video && *w == width);
        if !reuse {
            self.decoder = None;
            self.decoder = Some((video.clone(), width, Decoder::open(&video, Some(width))?));
        }
        let (Some((_, _, decoder)), Some(imaging)) = (&mut self.decoder, &self.imaging) else {
            unreachable!("both set above");
        };
        let mut order: Vec<usize> = (0..times.len()).collect();
        order.sort_by(|&a, &b| times[a].total_cmp(&times[b]));
        let mut images = vec![(String::new(), String::new()); times.len()];
        for k in order {
            let asked = times[k];
            let frame = decoder.frame_at(asked.clamp(0.0, duration.max(0.0)))?;
            let jpeg = imaging.jpeg(&imaging.scaled(&frame, width)?)?;
            images[k] = (
                format!(
                    "frame at {:.3} s ({}; asked {asked})",
                    frame.t,
                    clock(frame.t)
                ),
                b64::encode(&jpeg),
            );
        }
        let mut content = Vec::new();
        for (label, data) in images {
            content.push(text(&label));
            content.push(json!({ "type": "image", "data": data, "mimeType": "image/jpeg" }));
        }
        Ok(content)
    }

    fn audio_curve(&self, args: &Value) -> Result<Value, String> {
        let clip = &self.library.clips[self.find(args)?];
        let key = match &args["track"] {
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            _ => clip
                .game_track()
                .map_or_else(|| "0".to_owned(), |t| t.index.to_string()),
        };
        let track = clip.track(&key).ok_or_else(|| {
            let known: Vec<String> = clip
                .audio
                .iter()
                .map(|t| format!("{} ({})", t.index, t.role))
                .collect();
            format!("no audio track `{key}`; the clip has: {}", known.join(", "))
        })?;
        let hz = f64::from(clip.hz);
        let from = args["from"].as_f64().unwrap_or(0.0).max(0.0);
        let to = args["to"]
            .as_f64()
            .unwrap_or(clip.duration)
            .min(clip.duration);
        let first = (from * hz).floor() as usize;
        let last = ((to * hz).ceil() as usize).min(track.loudness.len());
        let values = track
            .loudness
            .get(first..last.max(first))
            .unwrap_or_default();
        // Fewer points: the loudest value of each group.
        let out_hz = args["hz"]
            .as_f64()
            .filter(|&h| h > 0.0 && h < hz)
            .unwrap_or(hz);
        let group = ((hz / out_hz).round() as usize).max(1);
        let dbfs: Vec<i64> = values
            .chunks(group)
            .map(|c| db(c.iter().copied().max().unwrap_or(0)).round() as i64)
            .collect();
        Ok(json!({
            "id": clip.id,
            "track": track.index,
            "name": track.name,
            "role": track.role,
            "hz": hz / group as f64,
            "from": round3(first as f64 / hz),
            "dbfs": dbfs,
            "notes": "RMS level in dBFS; -60 means silent or muted. Each value covers 1/hz seconds starting at from.",
        }))
    }

    fn overlaps(&self, args: &Value) -> Result<Value, String> {
        let i = self.find(args)?;
        let clips = &self.library.clips;
        let all = overlap::overlaps(clips);
        let groups = overlap::groups(clips);
        let group = groups.iter().find(|g| g.clips.contains(&i)).map(|g| {
            json!({
                "id": g.id,
                "from": round3(g.from),
                "to": round3(g.to),
                "clips": g.clips.iter().map(|&c| clips[c].id.as_str()).collect::<Vec<_>>(),
            })
        });
        let clip = &clips[i];
        Ok(json!({
            "id": clip.id,
            "session": clip.session,
            "session_range": clip.session_range().map(|(_, a, b)| json!([round3(a), round3(b)])),
            "overlaps": overlap::to_json(clips, &all[i]),
            "group": group,
            "notes": "shared: this clip's span that the other clip also has. The other clip's time = this clip's time + offset.",
        }))
    }

    fn analyze(&mut self, args: &Value) -> Result<Value, String> {
        let i = self.find(args)?;
        let clip = &self.library.clips[i];
        let force = args["force"].as_bool().unwrap_or(false);
        if clip.analysis.is_some() && !force {
            return Ok(json!({
                "id": clip.id,
                "already": true,
                "picture": clip.analysis.as_ref().map(analysis::describe),
            }));
        }
        if self.imaging.is_none() {
            self.imaging = Some(Imaging::new()?);
        }
        let imaging = self.imaging.as_ref().ok_or("no imaging")?;
        let analysed = analysis::analyze(clip, imaging)?;
        analysis::save(&analysed)?;
        let picture = analysed.analysis.as_ref().map(analysis::describe);
        let id = analysed.id.clone();
        let saved = analysed.sidecar.is_some();
        self.library.clips[i] = analysed;
        Ok(json!({
            "id": id,
            "picture": picture,
            "saved": saved,
            "notes": if saved { "Stored in the clip's sidecar; timeline now has the cut and loud events." } else { "The clip has no sidecar, so the result is not stored." },
        }))
    }
}

fn time_arg(args: &Value, key: &str) -> Result<Option<f64>, String> {
    match args[key].as_str() {
        None => Ok(None),
        Some(s) => parse_utc(s).map(Some).ok_or_else(|| {
            format!("`{key}` must be a date or time like 2026-10-17 or 2026-10-17T21:00:00Z")
        }),
    }
}

fn tools() -> Vec<Value> {
    let id = json!({ "type": "string", "description": "Clip id from list_clips (a unique prefix of 6+ characters, or the file name, also works)." });
    vec![
        json!({
            "name": "list_clips",
            "description": "List saved clips with game, time, length, session range, event counts and overlap group. Filters combine.",
            "inputSchema": { "type": "object", "properties": {
                "game": { "type": "string", "description": "Part of the game name or exe, any case." },
                "since": { "type": "string", "description": "Clips that start at or after this date or UTC time (2026-10-17 or 2026-10-17T21:00:00Z)." },
                "until": { "type": "string", "description": "Clips that start before this date or UTC time." },
                "has_event": { "type": "string", "description": "Only clips with an event of this kind: marker, loud, cut, kill, …" },
                "order": { "type": "string", "enum": ["newest", "oldest"], "description": "Default newest first." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Default 50." }
            } }
        }),
        json!({
            "name": "clip",
            "description": "Everything the clip's sidecar says (game, UTC and session timing, video format, audio tracks and roles, events, analysis), its file paths and overlaps. Loudness curves are summarised; use audio_curve for them.",
            "inputSchema": { "type": "object", "properties": { "id": id }, "required": ["id"] }
        }),
        json!({
            "name": "timeline",
            "description": "The clip's moments in time order, in clip seconds: markers, loud audio (with the peak to look at), scene cuts and any game events, each with source and confidence, plus a picture summary if analysed.",
            "inputSchema": { "type": "object", "properties": { "id": id }, "required": ["id"] }
        }),
        json!({
            "name": "frames",
            "description": "Decoded video frames at the given clip times, as JPEG images. Ask only for the moments you need.",
            "inputSchema": { "type": "object", "properties": {
                "id": id,
                "times": { "type": "array", "items": { "type": "number" }, "minItems": 1, "maxItems": MAX_FRAMES, "description": "Clip times in seconds." },
                "width": { "type": "integer", "minimum": 64, "maximum": 1920, "description": "Image width in pixels, default 768." }
            }, "required": ["id", "times"] }
        }),
        json!({
            "name": "audio_curve",
            "description": "Loudness of one audio track over time in dBFS (10 values per second by default).",
            "inputSchema": { "type": "object", "properties": {
                "id": id,
                "track": { "type": ["string", "integer"], "description": "Track index or role: mix, game, voice, other, mic, system. Default: the game track." },
                "from": { "type": "number", "description": "Start, clip seconds." },
                "to": { "type": "number", "description": "End, clip seconds." },
                "hz": { "type": "number", "description": "Fewer values per second (each the loudest of its group)." }
            }, "required": ["id"] }
        }),
        json!({
            "name": "overlaps",
            "description": "Clips that cover the same moments of the same game session as this one, with the shared span and the time offset between them, and the clip's overlap group.",
            "inputSchema": { "type": "object", "properties": { "id": id }, "required": ["id"] }
        }),
        json!({
            "name": "analyze",
            "description": "Run the after-save analysis on a clip (thumbnail, scene cuts, brightness and motion per second, dark and static stretches, loud moments as events) and store it in its sidecar. Takes a few seconds per minute of video.",
            "inputSchema": { "type": "object", "properties": {
                "id": id,
                "force": { "type": "boolean", "description": "Run again even if already analysed." }
            }, "required": ["id"] }
        }),
    ]
}

fn text(s: &str) -> Value {
    json!({ "type": "text", "text": s })
}

fn json_text(v: &Value) -> Value {
    text(
        &serde_json::to_string_pretty(v)
            .map_or_else(|e| e.to_string(), |s| analysis::compact_number_arrays(&s)),
    )
}

fn result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn server(dir: &Path) -> Server {
        Server {
            library: Library::scan(&[dir.to_owned()], &dir.join("sessions")),
            journal_dir: dir.join("sessions"),
            imaging: None,
            decoder: None,
        }
    }

    fn call(s: &mut Server, id: i64, method: &str, params: Value) -> Value {
        s.handle(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .unwrap()
    }

    fn tool(s: &mut Server, name: &str, args: Value) -> (Value, bool) {
        let r = call(
            s,
            9,
            "tools/call",
            json!({ "name": name, "arguments": args }),
        );
        let content = &r["result"]["content"][0]["text"];
        let is_error = r["result"]["isError"].as_bool().unwrap_or(false);
        let text = content.as_str().unwrap_or_default();
        (
            serde_json::from_str(text).unwrap_or(Value::String(text.into())),
            is_error,
        )
    }

    fn library_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rec-cli-mcp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, id, from, game) in [
            (
                "a",
                "01AAAAAAAAAAAAAAAAAAAAAAAA",
                100.0,
                [0u8, 0, 200, 0, 0],
            ),
            ("b", "01BBBBBBBBBBBBBBBBBBBBBBBB", 110.0, [0u8; 5]),
        ] {
            let sidecar = json!({
                "rec": 1,
                "clip": { "id": id, "file": format!("{name}.mp4"), "duration": 30.0, "trigger": "hotkey", "saved_at": 29.0 },
                "game": { "name": "Team Fortress 2", "exe": "tf_win64.exe", "steam_appid": 440 },
                "time": { "start_utc": "2026-10-18T01:00:00Z", "utc_offset_min": -240, "session": "01S", "session_offset": from },
                "audio": [ { "index": 0, "name": "Game", "role": "game", "loudness": game } ],
                "events": [ { "t": 2.0, "kind": "marker", "src": "hotkey", "conf": 1 } ],
                "loudness": { "hz": 10, "scale": "x" }
            });
            std::fs::write(dir.join(format!("{name}.mp4")), b"not a video").unwrap();
            std::fs::write(dir.join(format!("{name}.rec.json")), sidecar.to_string()).unwrap();
        }
        dir
    }

    #[test]
    fn speaks_the_protocol() {
        let dir = library_dir("protocol");
        let mut s = server(&dir);
        let init = call(
            &mut s,
            1,
            "initialize",
            json!({ "protocolVersion": "2025-03-26", "capabilities": {} }),
        );
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "rec");
        let other = call(
            &mut s,
            2,
            "initialize",
            json!({ "protocolVersion": "1999-01-01" }),
        );
        assert_eq!(other["result"]["protocolVersion"], VERSIONS[0]);
        assert!(
            s.handle(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
                .is_none()
        );
        assert_eq!(call(&mut s, 3, "ping", json!({}))["result"], json!({}));
        let list = call(&mut s, 4, "tools/list", json!({}));
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "list_clips",
                "clip",
                "timeline",
                "frames",
                "audio_curve",
                "overlaps",
                "analyze"
            ]
        );
        assert_eq!(call(&mut s, 5, "nope", json!({}))["error"]["code"], -32601);
        assert_eq!(
            call(&mut s, 6, "tools/call", json!({ "name": "nope" }))["error"]["code"],
            -32602
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tools_answer_from_sidecars() {
        let dir = library_dir("tools");
        let mut s = server(&dir);
        let (list, err) = tool(&mut s, "list_clips", json!({ "game": "fortress" }));
        assert!(!err);
        assert_eq!(list["total"], 2);
        assert_eq!(list["clips"][0]["overlap_group"], "g1");
        let (list, _) = tool(&mut s, "list_clips", json!({ "has_event": "loud" }));
        assert_eq!(list["total"], 1, "{list}");
        assert_eq!(list["clips"][0]["id"], "01AAAAAAAAAAAAAAAAAAAAAAAA");
        let (list, _) = tool(&mut s, "list_clips", json!({ "since": "2026-10-19" }));
        assert_eq!(list["total"], 0);
        let (_, err) = tool(&mut s, "list_clips", json!({ "since": "soon" }));
        assert!(err);

        let (clip, _) = tool(&mut s, "clip", json!({ "id": "01AAAAAA" }));
        assert_eq!(clip["audio"][0]["loudness"]["omitted"], "use audio_curve");
        assert_eq!(clip["overlaps"][0]["id"], "01BBBBBBBBBBBBBBBBBBBBBBBB");
        let (t, _) = tool(&mut s, "timeline", json!({ "id": "a.mp4" }));
        let kinds: Vec<&str> = t["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["loud", "marker"]);
        let (curve, _) = tool(
            &mut s,
            "audio_curve",
            json!({ "id": "a", "track": "game", "hz": 5 }),
        );
        assert_eq!(curve["dbfs"], json!([-60, -13, -60]));
        assert_eq!(curve["hz"], 5.0);
        let (o, _) = tool(&mut s, "overlaps", json!({ "id": "01BBBBBB" }));
        assert_eq!(o["overlaps"][0]["offset"], 10.0);
        assert_eq!(o["group"]["clips"].as_array().unwrap().len(), 2);
        let (msg, err) = tool(&mut s, "timeline", json!({ "id": "zzz" }));
        assert!(err && msg.as_str().unwrap().contains("no clip"), "{msg}");
        let (msg, err) = tool(&mut s, "frames", json!({ "id": "a", "times": [] }));
        assert!(err, "{msg}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
