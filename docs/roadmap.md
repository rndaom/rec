# Roadmap

## M0: Foundation ✅
- [x] Cargo workspace, CI, license, docs
- [x] `rec-core`: config (TOML, defaults, clamping, atomic save), hotkey parsing + conflict list, IPC protocol, timestamps
- [x] `rec-core::replay`: encoded-packet replay buffer (keyframe-aligned, GOP eviction, byte cap, snapshots)
- [x] `rec` CLI client
- [x] `recd` stub: load/validate/write config, warn about hotkey conflicts

## M1: It clips
The goal of M1 is that pressing the hotkey in a borderless game saves a playable MP4 with sound.
- [ ] `recd`:
  - [ ] Windows subsystem, single instance, tray icon with health state (recording / idle / error + reason)
  - [ ] Named-pipe IPC server implementing `rec-core::ipc`
  - [ ] Game detection: foreground window hook, fullscreen heuristics, allow/ignore lists
  - [ ] Idle teardown when no game is running
- [ ] Capture:
  - [ ] WGC window and display capture, D3D11 device on the adapter that owns the output (LUID)
  - [ ] Fixed encode tick
  - [ ] DDA fallback
- [ ] GPU BGRA→NV12 (D3D11 video processor)
- [ ] Media Foundation hardware encoder: H.264 + HEVC, IDR every 1 s, no B-frames, capped VBR
- [ ] Audio:
  - [ ] WASAPI endpoint loopback + mic, QPC timestamps, silence fill
  - [ ] Media Foundation AAC encoding
- [ ] Save:
  - [ ] Replay buffer wired up
  - [ ] Save hotkey (`RegisterHotKey`)
  - [ ] Fragmented-MP4 writer with multi-track audio
- [ ] Feedback: "clip saved" sound + non-activating toast
- [ ] Benchmark harness: PresentMon runs scripted against a test game, results in `bench/`

## M2: It's good
- [ ] Per-process audio tracks: game, voice chat, mic, other apps, plus a mixed track
- [ ] Native NVENC / AMF / oneVPL backends; AV1; encoder capability probing and fallback chain
- [ ] HDR: FP16 capture → HEVC Main10 PQ with HDR10 metadata, or tone-map to SDR
- [ ] Quick-panel overlay (DirectComposition, excluded from capture), keyboard and mouse navigation
- [ ] Per-game profiles; post-roll; extra clip-length hotkeys; marker hotkey; controller combo
- [ ] Folder per game, filename template, storage quota with auto-prune (starred clips kept)
- [ ] Windows 10 path: DDA cropped to the game window; hybrid-GPU handling; device-lost / driver-update / resolution-change recovery without losing the buffer

## M3: Library
- [ ] `rec-ui` (Tauri 2 + Svelte 5): settings, clip library, lossless trim, star / delete (Recycle Bin)
- [ ] "Discord-ready" export: H.264 MP4 re-encoded to fit a size limit; copy-file-to-clipboard; drag out
- [ ] Marker timeline

## M4: Ship it
- [ ] Installer, code signing, start with Windows, clean uninstall
- [ ] Opt-in update check; updates never applied while a game is running
- [ ] Disk segment ring + long buffers; screenshot hotkey; post-save script hook; Stream Deck plugin
- [ ] Battery-aware mode for laptops and handhelds

## Later
- Local auto-clipping through official game APIs (CS2 / Dota 2 Game State Integration, League Live Client Data API) and a local voice trigger
- Vertical (9:16) export, GIF export
- Opt-in hook capture for true-exclusive-fullscreen games without anti-cheat
