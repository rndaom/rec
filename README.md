# rec

A free, open-source instant-replay recorder for Windows that stays out of your game's way.

rec runs quietly in the background and keeps the last minute (or however long you choose) of your game. Press **Alt+F8** and that moment is saved as a clip. That's the whole idea. It works in any game or app, needs no account, never uploads anything, and is built so your game keeps its frame rate.

> **Status: pre-alpha (milestone 1 done).** rec records games, keeps a replay buffer and saves clips with sound. There is no settings UI yet (edit the config file from the tray menu), no installer, and only one combined audio track. See the [roadmap](docs/roadmap.md).

## Measured cost
These are early numbers from one machine (Ryzen 7 9800X3D, RX 7800 XT, Windows 11 25H2). They were taken while recording a live Deadlock match at 1080p60, HEVC, 18 Mbit/s, with a 60 s buffer. Proper PresentMon frame-time benchmarks are still to be run.

`rec-bench` ([bench/README.md](bench/README.md)) measures these costs: it compares PresentMon captures with `recd` off and on and reads `recd`'s CPU and memory. The figures in the table below were taken before it existed.

| | `recd` |
|---|---|
| CPU | 0.34% of total (5.5% of one core) |
| Memory | 127 MB private, including the replay buffer |
| Binary size | 1.2 MB |
| Saving a 15 s clip | Remux on a low-priority thread, no re-encode |

## Goals
- **Game first.**
  - Hardware encoding (NVIDIA, AMD, Intel) with frames that never leave the GPU.
  - A compressed in-memory replay buffer, and clips saved without re-encoding.
  - Near-zero cost when no game is running.
  - Measured impact published for every release.
- **Works everywhere.** Any game, any app, or the whole desktop. Windows 10 and 11. No code injection, so it is safe with anti-cheat (Vanguard, EAC, BattlEye, FACEIT).
- **Never silently off.** The tray icon always shows whether rec is recording and, if it isn't, why.
- **Your audio, your way.** Game, voice chat, mic and other apps on separate tracks, plus a mixed track.
- **Local and private.** Captures only the game window by default. No accounts, no telemetry, no uploads.
- **Quick control in game.** A small overlay (Alt+F7) for clip length, quality, audio toggles and destination.
- **Tidy storage.** A folder per game, readable filenames, and an optional size cap that removes old clips automatically.

## Layout
```
crates/
  rec-core/   platform-independent core: config, hotkeys, IPC protocol, replay buffer
  rec-video/  Windows.Graphics.Capture -> GPU colour conversion -> hardware encoder
  rec-audio/  WASAPI loopback and microphone -> AAC
  rec-mux/    fragmented MP4 writer (H.264 / HEVC / AV1, multi-track AAC)
  recd/       background service: tray, hotkeys, game detection, sessions, saving
  rec-cli/    `rec` command-line control (rec save 30, rec status, rec digest, rec mcp, ...)
  rec-library/ saved clips for AI tools: sidecars, journals, overlaps, frame decoding, analysis
  rec-bench/  benchmark harness: PresentMon with and without recd, recd idle cost
bench/
  README.md     how to run a fair benchmark and read the results
  results/      published summaries (raw PresentMon CSVs are git-ignored)
docs/
  research.md   competitor and technical research
  decisions.md  architecture decisions and why
  roadmap.md    milestones
  ai-clips.md   clips an AI can understand: sidecars, digest bundles, MCP server
```

## Building
Requires Windows 10/11 x64 and the stable Rust toolchain (MSVC). `rust-toolchain.toml` pins the rest.

```bash
cargo build --release
```

```bash
cargo test
```

## Running
Start `target\release\recd.exe`. A dot appears in the notification area:
- **Red:** recording.
- **Grey ring:** waiting for a game.
- **Amber:** paused.
- **Orange ring:** a problem. Hover over it or open its menu to see why.

Start a game and press **Alt+F8** to save the last 60 seconds to `Videos\rec\<game>\`.

## Using the CLI
```bash
rec status
```

```bash
rec save 30
```

`rec help` lists every command. Configuration lives at `%APPDATA%\rec\config.toml`; `rec config` prints the exact path.

To cap the space clips use, set `quota_gb` under `[output]`. rec then deletes its oldest clips after each save. `rec star` keeps the newest clip (or `rec star <clip>` a given one) out of that, and so does making a clip read-only. In game, "Star last clip" in the quick panel (Alt+F7) does the same for the clip you just saved, or set a hotkey for it: `star_last = "Alt+F9"` under `[hotkeys]` (there is no default). `rec prune --dry-run` lists what would go. Only clips rec saved are ever counted or deleted.

## Clips for AI tools
Every clip gets a `<clip>.rec.json` sidecar with the game, when it was recorded, markers, and each audio track's role and loudness. Two commands make clips readable by a model:

```bash
rec digest "%USERPROFILE%\Videos\rec\Team Fortress 2"
```

writes a bundle per clip (summary, timeline, stills and labelled contact sheets) and an `index.json` that groups overlapping clips.

```bash
claude mcp add rec -- rec mcp
```

lets Claude Code, or any MCP client, list clips, read timelines and decode just the frames it needs. Everything stays on this machine. See [docs/ai-clips.md](docs/ai-clips.md).

## License
Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
