# AI-ready clips (proposal)

Status: phase A built on branch `m2` (2026-10-17): the sidecar v1, markers, the session journal and loudness curves. See [Sidecar v1 as built](#sidecar-v1-as-built). Phases B to D are still proposals.

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
- **Not yet written:** `game.profile`, `player`, `analysis`, `privacy` and `video.vfr`/`dropped_frames`. They come with per-game profiles and layer 2.
- **`clip.trigger`** is `hotkey`, `panel`, `tray` or `cli`.
- **Events** are only markers for now: `{ "t", "kind": "marker", "src": "hotkey" | "cli", "conf": 1 }`.
- **`time.utc_offset_min`** sits next to `start_utc` (which is UTC, with a `Z`).

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

**`rec digest <clip|folder> [--out dir]`** writes a self-contained bundle that any model can read:
- `clip.rec.json`
- `summary.md`: a plain-language timeline, for example "0:21.2 kill (sniper rifle, crit) · 0:22.4 loud · 0:24.9 marker".
- `frames/`: stills at each event and marker, plus one every N seconds, named by clip time (`t021.18.jpg`) and sized for vision models.
- `sheet-*.jpg`: a few labelled contact sheets, capped at a small number of images. The TF2 reviewers hit request limits with 319 image reads.

For a folder, `index.json` lists every clip with game, session, range, event counts and an overlap group.

**`rec mcp`** is a stdio MCP server, local only, so Claude or another agent can query the library directly instead of reading files. It exposes these tools:
- `list_clips(game?, since?, has_event?)`
- `clip(id)`
- `timeline(id)`
- `frames(id, times[], width?)`: decodes on demand, so the agent asks only for the frames it needs
- `audio_curve(id, track)`
- `search(text)`: over transcripts and events
- `overlaps(id)`

With this in place, the TF2 job's first hour becomes `list_clips(game: "Team Fortress 2", has_event: "kill")`, then `timeline` and a few `frames` calls per candidate.

## Phasing

| Phase | Fits in | Scope |
|---|---|---|
| A | M2 (next to the marker hotkey and per-game profiles) | Sidecar v1 with layer 0, marker hotkey, session journal, layer 1 loudness curves, `player.names` and `hud`/`sources` slots in profiles |
| B | M3 (with `rec-ui` and the library) | Layer 2 thumbnails, cuts and quality; `rec digest`; `rec mcp`. The library UI reads the same sidecars |
| C | M3+ (opt-in) | Local transcripts; optional local captioning endpoint |
| D | Later (per game) | Sources: CS2 and Dota GSI, League, Source console log, Minecraft log, HUD profile engine (starting with TF2), then demo parsers |

Phase A is small and on the save path, so it is the place to start. Every later phase depends on its ids, times and markers. It also means every clip the user records from now on is already AI-ready, even before any analysis exists.

## Open questions

- **Embed in the MP4?** A copy in `moov/udta` would survive uploads and renames, as the SteelSeries tag did. `moov` is written at the first keyframe, so this needs either a reserved `free` box that `finish()` overwrites (it already patches durations) or a trailing box. Check `rec-mux/src/writer.rs` before choosing. MP4 chapters for markers would also show up in ordinary players.
- **Can trim and export in rec-ui keep the sidecar?** They should copy it, shift the times and set `parent`.
- **Sidecar clutter.** Do `<clip>.rec.json` and the `<clip>.rec/` folder sit next to clips, or go in a hidden `.rec` folder per directory? Next to clips is simpler and survives copying a single clip. A setting can hide them.
