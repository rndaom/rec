# Roadmap

## M0: Foundation ✅
- [x] Cargo workspace, CI, license, docs
- [x] `rec-core`: config (TOML, defaults, clamping, atomic save), hotkey parsing + conflict list, IPC protocol, timestamps
- [x] `rec-core::replay`: encoded-packet replay buffer (keyframe-aligned, GOP eviction, byte cap, snapshots)
- [x] `rec` CLI client
- [x] `recd` stub: load/validate/write config, warn about hotkey conflicts

## M1: It clips ✅
Pressing the hotkey in a game saves a playable MP4 with sound. Verified on a live Deadlock match (RX 7800 XT, 1080p60 HEVC):
- 15.7 s clip, decodes cleanly, game audio present.
- `recd` used 0.34% of total CPU (5.5% of one core).
- 127 MB private memory, including a 39 MB replay buffer.

**`recd`**
- [x] Windows subsystem, single instance, tray icon with health state (recording / idle / paused / error + reason)
- [x] Named-pipe IPC server implementing `rec-core::ipc` (`rec status`, `rec save 30`, `rec pause`, …)
- [x] Game detection: out-of-context foreground hook, fullscreen heuristics, store-library paths, allow/ignore/private lists
- [x] Friendly game names
- [x] Idle teardown when the game closes

**`rec-video`**
- [x] WGC window and display capture with the border turned off
- [x] D3D11 device on the adapter that owns the output
- [x] Fixed encode tick
- [x] GPU BGRA→NV12 + scaling/letterbox
- [x] Media Foundation hardware encoder: H.264 + HEVC, IDR every 1 s, no B-frames, peak-constrained VBR

**`rec-audio`**
- [x] WASAPI loopback + mic, converted to 48 kHz stereo by the audio engine
- [x] QPC timeline with silence fill and drift correction
- [x] Follows default-device changes
- [x] Media Foundation AAC

**`rec-mux`**
- [x] Fragmented MP4 (H.264 / HEVC / AV1 + multi-track AAC)
- [x] Crash-safe
- [x] Streaming post-roll

**Saving and feedback**
- [x] Save hotkey and post-roll
- [x] Low-priority (EcoQoS) save thread, folder per game, filename template
- [x] "Clip saved" sound + non-activating toast, excluded from capture

**Moved to M2**
- DDA fallback; the benchmark harness

## M2: It's good
- [ ] Benchmark harness: PresentMon/CapFrameX runs with and without `recd`, results in `bench/` and the README
  - Harness done (`rec-bench`, see `bench/README.md`); published numbers pending a run on real hardware (NVIDIA, AMD, Intel, hybrid laptop)
- [x] Per-process audio tracks (game, voice chat, mic, other apps) + mixed track (`AudioSource::Process`)
- [ ] Native NVENC / AMF / oneVPL backends; AV1; encoder capability probing and fallback chain
- [ ] HDR: FP16 capture → HEVC Main10 PQ with HDR10 metadata, or tone-map to SDR
- [ ] Quick-panel overlay (DirectComposition, excluded from capture), keyboard and mouse navigation
- [ ] Per-game profiles; extra clip-length hotkeys in the UI; marker hotkey; controller combo
- [ ] Storage quota with auto-prune (starred clips kept)
- [ ] Windows 10 path: DDA fallback (monitor capture without the yellow border)
- [x] Exclusive-fullscreen black-frame detection with a "switch to borderless" hint
- [x] Hybrid-GPU handling; device-lost / driver-update / resolution-change recovery without losing the buffer (`recd/src/recover.rs`; the GPU-switch path is not yet tested on a hybrid laptop)
- [ ] Adaptive GPU priority when frames drop under full GPU load
- [x] Cached silent AAC packet instead of encoding silence

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
