# rec

A free, open-source instant-replay recorder for Windows that stays out of your game's way.

rec runs quietly in the background and keeps the last minute (or however long you choose) of your game. Press **Alt+F8** and that moment is saved as a clip. That's the whole idea. It works in any game or app, needs no account, never uploads anything, and is built so your game keeps its frame rate.

> **Status: pre-alpha.** The foundation is in place (config, hotkeys, IPC protocol, replay buffer, CLI). Capture and encoding are next. See the [roadmap](docs/roadmap.md).

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
  recd/       background service: capture, encode, hotkeys, tray, overlay
  rec-cli/    `rec` command-line control (rec save 30, rec status, ...)
docs/
  research.md   competitor and technical research
  decisions.md  architecture decisions and why
  roadmap.md    milestones
```

## Building
Requires Windows 10/11 x64 and the stable Rust toolchain (MSVC). `rust-toolchain.toml` pins the rest.

```bash
cargo build --release
```

```bash
cargo test
```

## Using the CLI
```bash
rec status
```

```bash
rec save 30
```

`rec help` lists every command. Configuration lives at `%APPDATA%\rec\config.toml`; `rec config` prints the exact path.

## License
Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
