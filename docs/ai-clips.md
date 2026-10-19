# AI-ready clips

Status: phases A and B built on branch `m2` (2026-10-17). Phase A: the sidecar v1, markers, the session journal and loudness curves (see [Sidecar v1 as built](#sidecar-v1-as-built)). Phase B: layer 2 analysis (`rec analyze`), `rec digest` and `rec mcp` (see [How an AI uses it](#how-an-ai-uses-it) and [Phase B as built](#phase-b-as-built)). Phases C and D are still proposals.

**Goal:** a clip saved by rec should be easy for an AI to understand: what game it is, what happened and when, which moments matter, and how it relates to the other clips. This must work for every game, need no setup, and stay within D3 (no injection), D10 (local, no GPL) and D11 (no cost during gameplay).

## What the TF2 frag-video session showed

A Claude session turned 281 raw TF2 clips (SteelSeries Moments recordings) into a frag movie. Almost all of the effort went into working out what each clip contained, not into editing.

| Work the AI had to do | Cost | How it went |
|---|---|---|
| Find the game and date from filenames, and probe 281 files for resolution and fps | small | Resolutions and frame rates were mixed: 1080p, 720p and 1128x640, and some clips had a variable frame rate |
| Match the kill sound in the audio | one try | Failed (correlation about 0.1) and was abandoned |
| Scan the killfeed for the local player's highlighted boxes (OpenCV, 20 fps) | about 3.5 min, several fixes | Merged kills that landed close together, so the two best clips were undercounted (3 kills instead of 5, and 5 instead of 6) |
| OCR the killfeed text to classify events and find the player | about 3.6 min | Names came out garbled ("ky vogma mor"). The player had 4 aliases, so the list had to be curated by hand |
| Make contact sheets, then have 3 Sonnet reviewers rate 128 clips | 12–44 min per batch, about 400k output tokens, 319 image reads in one batch | Hit image request limits. Map identified for only 29 of 128 clips. Class and weapon were guessed from the viewmodel |
| Count killfeed boxes at 60 fps to find the exact kill frame | per clip | Needed because the event list was too coarse |
| Find overlapping clips and `_trim` duplicates | manual | 30 overlap groups. The same kill was placed in the edit twice and only caught at storyboard QA |

The SteelSeries clips already had a JSON blob in an MP4 tag: game, Steam app id, session id, trigger, audio track roles and a saved-clip marker time. **No step of the pipeline read it.** An AI only uses metadata it can find, so the format has to be obvious and documented.

Ranked by how much work it would have saved:
1. Timestamped events with a source and a confidence value
2. Marker and save times inside the clip
3. Session identity, overlap and trim lineage
4. Game context: map, character, player name
5. Capture facts: resolution, fps, whether the frame rate is variable, and which audio track is which
6. Ready-made stills at the event times
7. Simple quality signals: dark, loud, quiet, static

## Are demo files the answer?

For TF2 they are the gold standard: tick-exact kills, weapons, positions and names. They are the wrong foundation for rec, for three reasons:
- **Few games have them.** Source games, Dota 2, Rocket League and a few others do. Most games have nothing.
- **They must be on before playing.** None of the TF2 clips had a demo.
- **Something has to tie demo time to video time.** A demo has no idea which video frame matches tick 48213.

So rec should have one **game-neutral event timeline** that every clip gets. Any number of optional sources can write into it, and demos are just one of those sources. With zero sources, a clip still gets everything rec knows on its own.

## Design

Clips are understood in four layers. Each layer works without the ones below it.

```
Layer 0  Capture facts      every clip, every game   written by recd at save time (free)
Layer 1  Universal signals  every clip, every game   recorded alongside the buffer (near free)
Layer 2  Analysis           every clip, every game   after saving, in the background, opt-out
Layer 3  Game sources       per game, opt-in         official APIs and files only
                 ↓ all of them write into the same timeline
         <clip>.rec.json  →  `rec digest`  /  `rec mcp`  →  any AI
```

### The sidecar: `<clip>.rec.json`

The sidecar is written next to the clip with the same file stem. It is plain JSON with a version number. It is the canonical record. A later version can also embed a copy in the MP4 (see the open questions).

```jsonc
{
  "rec": 1,                                   // schema version
  "clip": {
    "id": "01JABC…",                          // ULID, stable when the file is renamed
    "file": "TF2_2026-10-17_21-04-33.mp4",
    "parent": null,                           // id of the source clip for trims and exports
    "duration": 30.017,
    "trigger": "hotkey",                      // hotkey | panel | cli | marker | auto:<source>
    "saved_at": 27.5                          // clip time at which the save was requested
  },
  "game": {
    "name": "Team Fortress 2",                // the friendly name rec shows
    "exe": "tf_win64.exe",
    "steam_appid": 440,                       // from the steamapps path and appmanifest, if found
    "profile": "tf2"                          // per-game profile in use, if any
  },
  "time": {
    "start_utc": "2026-10-17T21:04:03.112-04:00",   // wall clock of clip t = 0
    "session": "01JAB9…",                     // one id per continuous run of the game
    "session_offset": 1843.226                // clip t = 0 within that session
  },
  "video": { "codec": "hevc", "width": 2560, "height": 1440, "fps": 60, "vfr": false,
             "hdr": "off", "dropped_frames": 0 },
  "audio": [                                  // in file order, with a role an AI can rely on
    { "track": 1, "name": "Mixed",      "role": "mix" },
    { "track": 2, "name": "Game",       "role": "game" },
    { "track": 3, "name": "Voice chat", "role": "voice" },
    { "track": 4, "name": "Microphone", "role": "mic" }
  ],
  "player": { "names": ["RNDAOM"] },          // from the profile or a game source, never guessed
  "events": [                                 // clip seconds, sorted
    { "t": 21.180, "kind": "kill",   "src": "source:tf2-console", "conf": 1.0,
      "actor": "RNDAOM", "target": "xX_spy_Xx", "data": { "weapon": "sniperrifle", "crit": true } },
    { "t": 24.900, "kind": "marker", "src": "hotkey", "conf": 1.0 },
    { "t": 22.400, "kind": "loud",   "src": "audio:game", "conf": 0.7, "dur": 1.1 }
  ],
  "signals": {                                // compact curves for "where is the action"
    "hz": 10,
    "game_loudness": "base64 of u8[]",
    "mic_loudness":  "base64 of u8[]"
  },
  "analysis": { … },                          // filled in by layer 2, see below
  "privacy": { "window_title": false, "transcript": false }
}
```

**Event rules:**
- `kind` comes from a short shared vocabulary that is the same for every game. The list is: `marker`, `kill`, `assist`, `death`, `objective`, `round_start`, `round_end`, `match_end`, `ability`, `damage`, `speech`, `loud`, `cut`, `menu` and `scoreboard`.
- A game-specific kind is namespaced, for example `tf2:uber` or `cs2:bomb_planted`. A consumer that does not know the kind can still sort it and show it.
- `src` and `conf` matter. A kill from GSI has confidence 1.0. A kill from killfeed OCR might have 0.8. An AI can then trust the first and verify the second. The TF2 reviewers only agreed with the OCR because they were shown it.
- Events from before t = 0 are dropped. The TF2 pipeline needed a `t > 0.3` filter to drop killfeed entries that were already on screen when the clip started.

### Layer 0: capture facts (recd, at save time)

`SaveJob` and `Session` already hold almost all of this. What needs adding:
- **Clip id.** A new ULID for each saved clip.
- **Trigger.** A small enum passed through `app.rs:save`.
- **Steam app id.** Read from the exe path and `appmanifest_*.acf`.
- **Session id.** A new id each time the game target starts.
- **QPC → UTC mapping.** Taken once per session.
- **QPC base of the clip.** The pts of the first keyframe.
- **Audio roles.** Already implied by the `plan_audio` names; make them explicit.
- **Markers.** The marker hotkey, `Request::Mark` and `rec mark` add a QPC timestamp to the game session (not the recording `Session`, so markers survive a settings reload or a pipeline restart). Markers are written to every clip whose range covers them, so a marker also works as "keep this moment" when the save comes later.

All of this is written on the save thread, which already runs at EcoQoS. It adds microseconds to a save that takes milliseconds.

**Session journal.** recd also appends one JSON line per marker and save to `%LOCALAPPDATA%\rec\sessions\<session>.jsonl`. Absolute session offsets make overlapping clips and trims easy to detect: two clips overlap exactly when their session ranges intersect. The journal also tells an AI that a clip is, say, the 3rd of 7 from one evening.

The window title stays off by default. It can contain chat, DMs or server names, which the capture design deliberately keeps out of clips.

### Layer 1: universal signals (beside the buffer)

**Loudness per audio track at 10 Hz.** rec-audio already has the PCM before it is encoded. A peak or RMS value per 100 ms is about 10 bytes per second per track, kept in a ring that matches the replay length. The cost is a few hundred floats of maths per second, well inside D11.

Why it is worth having:
- Game loudness marks gunfights, explosions and quiet stretches.
- Mic loudness is a strong "the player reacted" signal in every game: shouting, laughing, swearing at a backstab.
- It is the cheapest highlight hint that works in every game.

Nothing visual is computed during gameplay. Even a GPU luma reduction is extra work in the game's frame budget, and layer 2 can do it after saving.

### Layer 2: analysis after saving (rec-ui or an idle worker)

This runs only when no game is in the foreground, or on demand, at EcoQoS, with Media Foundation and D3D11 decoding (no FFmpeg, per D4). The results go into `analysis` and `events`.

| Analysis | Output | Notes |
|---|---|---|
| Thumbnail and keyframe strip | `analysis.thumbs`: times, plus JPEGs in `<clip>.rec/` | Decode only IDR frames, which come every 1 s and need no reference frames, so this is cheap |
| Scene cuts, menus, scoreboard | `cut`, `menu` and `scoreboard` events | Frame difference plus "mostly static UI" heuristics |
| Brightness, motion, blur per second | `analysis.quality` | Turns the reviewers' "dark", "hazy" and "static" notes into numbers |
| Audio onsets and peaks | `loud` events | Built from the layer 1 curves, with no decoding needed |
| Speech transcript (opt-in) | `speech` events with text | Local whisper.cpp (MIT) on the mic and voice tracks only. Off by default |
| Data-driven HUD reader (per profile) | `kill`, `death` and other events with `src: "hud:<profile>"` and conf < 1 | See layer 3 |

Built in phase B, on demand only (`rec analyze`, `rec digest` and the `analyze` MCP tool): the thumbnail, scene cuts, brightness and motion, dark and static stretches, and loud events. Not built yet: the keyframe strip, menu and scoreboard detection, blur, transcripts and the HUD reader. Nothing starts analysis by itself yet. See [Phase B as built](#phase-b-as-built).

**Vision captioning is not built in.** rec does not ship a model. `rec digest` and `rec mcp` (below) give any AI exactly what it needs. If the user wants automatic captions, an optional setting can point at an OpenAI-compatible local endpoint, such as their own llama-server. It is off by default and never uses the cloud without opt-in (D10).

### Layer 3: game sources (opt-in, per game profile)

Every source must be official and local. That means reading a file the game writes, or taking a localhost API or HTTP post the game offers. No memory reading and no hooks (D3). Each source emits the same event schema. Live sources stamp events with QPC on arrival. Post-hoc sources such as demos are aligned through `start_utc` and then fine-tuned by matching their events to `loud` peaks.

| Source | Games | Type | Notes |
|---|---|---|---|
| Game State Integration | CS2, Dota 2 | Live HTTP post to localhost | Kills, rounds, bomb, health and weapon. Already in "Later" |
| Live Client Data API | League of Legends | Live, localhost:2999 | `EventTime`, kills, objectives, champion |
| Console log (`-condebug` or `con_logfile`) | TF2 and other Source games | Live tail of a file | "X killed Y with Z" lines, including crits. Needs a check against current TF2 output |
| Demo files | TF2, CS2, Dota 2, Deadlock, … | Post-hoc parse | Tick-exact. Parsers must be MIT or Apache, or a separate tool rec only calls |
| Game logs | Minecraft (`logs/latest.log`) and others | Live tail | Deaths, advancements, chat |
| Replay files | Rocket League, Fortnite | Post-hoc | Lower priority |
| **HUD profile** | **Any game** | Post-hoc OCR on the saved clip | A data file per game describing HUD regions, for example the killfeed rectangle at 1080p, the local-player highlight colour and the death-screen cue. One engine runs every profile. This is the TF2 session's pipeline, generalised and fixed: scan at the clip's real frame rate and keep separate boxes separate. Profiles are plain files users can share |

A per-game profile (the planned M2 feature) holds the source settings and `player.names`. That settles "which name is me" once, instead of guessing it from OCR.

## Sidecar v1 as built

What phase A writes differs from the sketch above in a few places:
- **Loudness** sits on each audio entry as `audio[i].loudness`, one number per 100 ms from t = 0. The top-level `loudness` object gives `hz` and `scale` (RMS, 0 = -60 dBFS or quieter, 255 = 0 dBFS, linear in dB). Muted inputs and near-silence read 0, matching what is encoded.
- **`video.hdr`** is a boolean.
- **Not yet written:** `game.profile`, `player`, `privacy` and `video.vfr`/`dropped_frames`. They come with per-game profiles. `analysis` is added by `rec analyze` (phase B).
- **`clip.trigger`** is `hotkey`, `controller`, `panel`, `tray` or `cli`.
- **Events** from recd are only markers: `{ "t", "kind": "marker", "src": "hotkey" | "controller" | "cli", "conf": 1 }`. `rec analyze` adds `cut` and `loud` events.
- **`time.utc_offset_min`** sits next to `start_utc` (which is UTC, with a `Z`).
- **`starred`** (top level, boolean) is set by `rec star` and `rec unstar`, and by recd's "Star last clip" (the `hotkeys.star_last` hotkey, no default, and the quick-panel row), which stars the clip recd saved last in this run. Quota pruning never deletes a starred clip (D13). Starring a clip that has no sidecar writes one: `rec star` builds it from the journal line, recd from the same facts kept from the save (layer 0 only: no `video`, no loudness, `audio: []`). `rec analyze` keeps the flag from the file on disk, so starring during analysis sticks.

`output.sidecar = false` turns the file off. The journal is written either way.

The session journal is `%LOCALAPPDATA%\rec\sessions\<session id>.jsonl`. Every line has `type` and `utc`:

| `type` | Other fields |
|---|---|
| `start` | `session`, `game`, `exe`, `steam_appid`, `utc_offset_min` |
| `marker` | `t` (seconds from session start), `src` |
| `clip` | `clip` (id), `file`, `from`, `to`, `trigger_t` (session seconds) |
| `end` | `t`, `why` |

A game session is one run of one game: same process id and exe path. It continues through pauses, settings reloads and pipeline restarts, and ends when the game closes, another game starts or rec stops.

Code: `crates/recd/src/journal.rs` (game sessions, ids, UTC, Steam app id), `crates/recd/src/sidecar.rs` (the file) and `crates/rec-audio/src/levels.rs` (the loudness meter).

## How an AI uses it

Three `rec` commands. The `rec-library` crate does the work, and nothing of it runs in recd (D11).

**`rec digest <clip|folder> [--out DIR] [--every SECONDS] [--width PX] [--no-analysis] [--no-frames]`** writes a self-contained bundle that any model can read without tools:
- `clip.rec.json`: the sidecar, with the layer 2 analysis.
- `summary.md`: the clip in plain language. It gives the game, local and UTC time, length and format, the clip's span in its session ("clip 3 of 7 saved in this session"), how and when it was saved, then a timeline ("**0:07.9** marker (rec mark)", "**0:22.4–0:23.5** loud game audio, peak -12 dBFS"), the level of each audio track, a picture summary, overlapping clips with their time offset, and tables of the stills and sheets.
- `frames/t021.18.jpg` …: stills at every event (a loud stretch at its peak), at the save request, every N seconds (default 5) and at the last frame. Each is named by its frame's clip time, 1024 px wide by default, JPEG quality 0.85. Stills less than 0.25 s apart are merged. Scene cuts get stills only when a clip has 12 or fewer.
- `sheet-1.jpg` …: up to 4 contact sheets of 16 tiles (4 × 384 px, about 1556 px wide, so vision models don't scale them down). Each tile's time and reason are written under the picture, never over the HUD. If more stills are taken than fit, every event still goes on a sheet and the clock stills are spread evenly over the rest.

Without `--out`, bundles go to `rec-digest\<clip name>\` next to the clips. A folder is searched recursively, skipping `rec-digest` and `*.rec` folders, and `index.json` lists every clip with:
- its id, file (relative, with `/`), bundle and `source` (`sidecar` or `journal`);
- game, `start_utc`, duration, `starred`, session, `session_range` and `session_position` (`[n, of]` from the journal);
- event counts, thumbnail, `overlap_group` and its overlaps.

`groups` lists each overlap group with its session span and clip ids. A single clip is digested with its folder as context, so its overlaps and session position still show. Analysis runs first on clips that don't have it yet (`--no-analysis` skips it).

Clips saved with `output.sidecar = false` are found through the session journal. They get their id, range, game, start time and markers, but no audio or video facts, and rec writes nothing next to them unless one is starred.

**`rec analyze <clip|folder> [--force]`** runs layer 2 on clips that have a sidecar and stores the result in it (see [Phase B as built](#phase-b-as-built)).

**`rec mcp [--dir DIR]...`** is a local MCP server on stdin and stdout: JSON-RPC 2.0, one message per line, protocol versions 2025-06-18, 2025-03-26 and 2024-11-05. By default it serves the clips folder from the config (`output.directory`, else `Videos\rec`) and the session journals. It rescans on every call, without re-reading unchanged sidecars, so new clips show up while it runs. Tools:
- `list_clips(game?, since?, until?, has_event?, starred?, order?, limit?)`: newest first by default. `has_event` also matches loud moments derived from the curves. `starred: true` lists only the clips the user starred.
- `clip(id)`: the sidecar plus file paths, overlaps and session position. Loudness curves are summarised (median and peak).
- `timeline(id)`: entries with `t`, `clock`, `kind`, `src`, `conf`, a plain-language `note`, and `dur` and `look_at` (the peak) for loud stretches, plus the picture summary.
- `frames(id, times[], width?)`: up to 8 JPEG images per call, 768 px wide by default, decoded on demand. The decoder stays open between calls, so stepping through one clip is quick.
- `audio_curve(id, track?, from?, to?, hz?)`: dBFS values. A lower `hz` keeps the loudest value of each group.
- `overlaps(id)`: overlapping clips with the shared span and time offset, and the overlap group.
- `analyze(id, force?)`: layer 2 for one clip.

`id` takes the ULID, a unique prefix of 6 or more characters, or the file name. `search(text)` waits for transcripts (phase C). To add the server to Claude Code: `claude mcp add rec -- rec mcp` (with `rec.exe` on the PATH, or its full path).

With this in place, the TF2 job's first hour becomes `list_clips(game: "Team Fortress 2", has_event: "kill")`, then `timeline` and a few `frames` calls per candidate. `kill` events need a phase D source; until then `marker` and `loud` are the events every clip has.

## Phase B as built

**Decoding** uses the Media Foundation source reader with a D3D11 device, so decoding runs on the GPU, with a software fallback. Its video processor converts to BGRA and scales on the GPU. A request up to 1.5 s after the current position decodes on, and anything else seeks to the IDR frame before it (one every second). WIC scales stills and writes JPEG, and Direct2D and DirectWrite draw the sheets. There is no FFmpeg, no GPL code and no network access (`MFSTARTUP_LITE`). `rec digest` and `rec analyze` run at EcoQoS.

**Layer 2** decodes the clip once at 256 px wide and looks at it 10 times a second, as a 64 × 36 grid of mean colours. It writes `analysis` into the sidecar:

| Field | Meaning |
|---|---|
| `v`, `tool`, `utc` | Format version (1), what wrote it and when |
| `hz` | 1: `brightness` and `motion` have one value per second |
| `brightness` | Mean luma (BT.709), 0–255 |
| `motion` | Mean absolute change of the picture (RGB, 0–255) between looks 0.1 s apart |
| `dark` | `[from, to]` stretches with mean luma under 20, 1 s or longer |
| `static` | Stretches with almost no change (under 1) for 2 s or more: paused, a menu or loading |
| `cuts` | Number of `cut` events |
| `thumb` | `{ t, file, width }`: `<clip>.rec/thumb.jpg` next to the clip, 480 px wide. It is taken at the first marker, else the loudest game moment, else the middle, and moved off dark or static frames |

It also adds events with conf < 1, which every run replaces:
- **`cut`** (`src: "analysis:video"`): the picture changes by at least 20, its colour histogram by at least 0.35, and the change is at least 3 times the median change within ±1 s. A cut is stamped at the first look after it, so up to 0.1 s late. It compares colour, not just luma: fake_game's blue and red frames have the same brightness.
- **`loud`** (`src: "audio:game"`, or `mix` or `system` when there is no game track, plus `audio:mic` and `audio:voice`): stretches well above the clip's own median level (at least 4 dB above it, and never below -40 dBFS) whose peak is 6 dB over that threshold or that last a second. At most 8 are kept for the game and 5 each for mic and voice, the loudest first. Fields: `dur`, `peak_t`, `peak_db`, `track`. Clips that have not been analysed get the same loud moments derived when their timeline is read.

The sidecar is replaced in one step (a temporary file, then a rename) and keeps every other field in its order. A clip without a sidecar never gets one.

**Measured** on this machine (RX 7800 XT):
- The two fake_game clips from the phase A live test (946 × 532 HEVC, sidecars written by recd) were used. All 14 colour flips in the 13.9 s clip were found as cuts, each within 0.083 s of the frame-exact flip time from ffmpeg. The clips' overlap (3.883 s, offset 10 s) matches the journal. With its sidecar deleted, the second clip is rebuilt from the journal with the same id, range, start time and marker.
- Analysis took 1.4 s for that 14 s clip, and 9.7 s for a synthetic 60 s 2560 × 1440 60 fps HEVC clip. Once analysed, digesting the synthetic clip (15 stills, 1 sheet) took 1.9 s.
- `rec mcp` returned 3 frames in 217 ms from a cold decoder, then about 80 ms per frame.

**Not verified:** real game footage (cut and static thresholds were only tuned on fake_game and synthetic grids), HDR10 clips (the video processor makes 8-bit BGRA without recd's tone mapping), AV1 clips, and loud moments in a real recording (fake_game is silent; unit tests cover them).

Code: `crates/rec-library` (`clip`, `journal`, `library`, `overlap`, `timeline`, `summary`, `decode`, `image`, `analysis`, `digest`) and `crates/rec-cli/src/mcp.rs`. `cargo test -p rec-library decode -- --ignored` with `REC_TEST_CLIP=<clip.mp4>` decodes a real clip.

## Phasing

| Phase | Fits in | Scope |
|---|---|---|
| A | M2 (next to the marker hotkey and per-game profiles) | Sidecar v1 with layer 0, marker hotkey, session journal, layer 1 loudness curves, `player.names` and `hud`/`sources` slots in profiles |
| B | Built in M2 (2026-10-17), ahead of `rec-ui` | Layer 2 thumbnails, cuts and quality; `rec digest`; `rec mcp`. The library UI (M3) will read the same sidecars through `rec-library` |
| C | M3+ (opt-in) | Local transcripts and the `search(text)` MCP tool; optional local captioning endpoint |
| D | Later (per game) | Sources: CS2 and Dota GSI, League, Source console log, Minecraft log, HUD profile engine (starting with TF2), then demo parsers |

Phase A is small and on the save path, so it is the place to start. Every later phase depends on its ids, times and markers. It also means every clip the user records from now on is already AI-ready, even before any analysis exists.

## Open questions

- **Embed in the MP4?** A copy in `moov/udta` would survive uploads and renames, as the SteelSeries tag did. `moov` is written at the first keyframe, so this needs either a reserved `free` box that `finish()` overwrites (it already patches durations) or a trailing box. Check `rec-mux/src/writer.rs` before choosing. MP4 chapters for markers would also show up in ordinary players.
- **Can trim and export in rec-ui keep the sidecar?** They should copy it, shift the times and set `parent`.
- **Sidecar clutter.** Do `<clip>.rec.json` and the `<clip>.rec/` folder sit next to clips, or go in a hidden `.rec` folder per directory? Next to clips is simpler and survives copying a single clip. A setting can hide them. Today analysis writes `<clip>.rec/thumb.jpg` next to the clip, and `rec digest` writes `rec-digest/` next to the clips unless given `--out`.
- **Who starts analysis?** Today it runs only on demand. recd could queue it after a save and run it once no game is in the foreground (D11), or rec-ui could run it when the library opens.
