# Working notes

Handoff file so any session can pick up quickly. Keep it short and current.

**Goal:** rec, a lightweight, open-source, anti-cheat-safe instant-replay clipper for Windows 10/11 (see README).

**Read first:** [docs/decisions.md](docs/decisions.md) (the why), [docs/roadmap.md](docs/roadmap.md) (the what), [docs/research.md](docs/research.md) (the evidence).

**Status (2026-10-20):** M1 done, verified on a live game. M2 is built (branch `m2`; open: published benchmark numbers and NVENC/oneVPL, both need other hardware). M3 is built on branch `m3`: `rec-ui` (settings, per-game profiles, clip library, player with the marker timeline, lossless trim, star, Recycle Bin delete, copy/drag out, Discord-ready export), D17.

| Crate | Role |
|---|---|
| `rec-core` | Config, hotkeys, IPC, replay buffer, media types, QPC clock, the clips folder (`store`: rec clips, stars, quota pruning) |
| `rec-video` | WGC → D3D11 NV12/P010 (video processor or HDR shaders) → AMF or MF hardware encoder |
| `rec-audio` | WASAPI → AAC |
| `rec-mux` | Fragmented MP4 |
| `recd` | Tray service |
| `rec-cli` | `rec`, including `rec digest`, `rec analyze` and the `rec mcp` server (`src/mcp.rs`) |
| `rec-library` | Saved clips for AI tools: sidecars, journals, overlaps, timelines, MF/D3D11 decoding, WIC stills and sheets, layer 2 analysis |
| `rec-bench` | Benchmark harness: PresentMon off/on runs, `recd` idle cost (see `bench/README.md`) |
| `rec-ui` | Settings and clip library app (Tauri 2 + Svelte 5; frontend in `ui/`), D17. Separate process, never controls recording |

**Next step:** M4 (installer, start with Windows, updates), or the open M2 hardware items and the M3 leftovers below. M2 as done, in order:
1. ~~Benchmark harness~~ (harness done; the first published run on real hardware is still to do)
2. ~~Per-process audio tracks~~ (done)
3. ~~Black-frame / exclusive-fullscreen detection~~ (done)
4. ~~Pipeline recovery~~ (done: failures, resolution changes and GPU moves restart only the video pipeline as a new `segment`; see `recd/src/recover.rs`)
5. ~~Quick-panel overlay~~ (done: `recd/src/panel.rs` model, `overlay.rs` window; Alt+F7 or the tray)
6. ~~HDR~~ (done: HDR10 or tone-map, decision D12)
7. ~~Native encoders + AV1~~ (AV1 and AMD AMF done; NVENC and oneVPL need NVIDIA / Intel hardware)
8. ~~AI-ready clips phase A~~ (done: `<clip>.rec.json` sidecar, marker hotkey and `rec mark`, session journal, loudness curves; see [docs/ai-clips.md](docs/ai-clips.md)). The marker hotkey has no default: set `hotkeys.add_marker`. Phase B is done too: `rec digest`, `rec analyze`, `rec mcp` and layer 2 analysis (thumbnail, scene cuts, brightness, motion, loud moments). Phases C and D (transcripts, game sources) are proposals.
9. ~~Storage quota with auto-prune~~ (done: `output.quota_gb`, `rec star` / `unstar` / `prune`, decision D13; `recd/src/prune.rs`, `rec-core/src/store.rs`). In game, "Star last clip" stars the clip recd saved last: a quick-panel row, and the `hotkeys.star_last` hotkey, which has no default.
10. ~~Controller combos + extra clip rows in the panel~~ (done: `recd/src/controller.rs`, decision D7; untested with a real controller)
11. ~~Win10 DDA fallback~~ (done: `rec-video/src/dda.rs`, `video.capture`, D14)
12. ~~Adaptive GPU priority~~ (done: `rec-video/src/priority.rs`, `video.gpu_priority`, D15; also `video.dropped_frames` in sidecars)
13. ~~Per-game profiles~~ (done: `rec-core/src/profile.rs`, D16, `rec profile list|show|path`, panel "Settings for" row)
14. Remaining: a real benchmark run (admin + NVIDIA/Intel/hybrid hardware), a real Win10 + HDR display + controller check

**M3 plan (branch `m3`, started 2026-10-20):**
- ~~`rec-library::trim`~~ (done: `trim::plan` / `trim::trim`): lossless trim by cutting rec's fragmented MP4 at GOP fragments (box level: codec- and HDR-agnostic), start snaps to the keyframe at or before it (IDR every 1 s), end exact to the frame; writes `<stem> trim.mp4` + sidecar (`parent` = source id, events/loudness shifted, `analysis` and `video.dropped_frames` dropped). Only rec's own fMP4 layout is accepted. `rec_core::store::new_id` (moved from recd) makes the id. `cargo test -p rec-library ffprobe -- --ignored` checks a trim with ffprobe.
- ~~`rec-library::export`~~ (done): "Discord-ready" H.264/AAC MP4 under a size limit (MF source reader -> sink writer, GPU end to end, bitrate from size/duration, retry lower if over), progress + cancel. Notes under "Export" below.
- ~~`crates/rec-ui`~~ (done, D17; Tauri 2 + Svelte 5, frontend in `ui/`): settings over `config.toml` and profiles via `save_keys` / `Profile::save_settings` then IPC `ReloadConfig`; library over `rec_library::Library` (thumbnails from analysis or made on demand), player over the asset protocol with the marker timeline (sidecar events + loudness), trim, star, delete to the Recycle Bin, copy file to clipboard, drag out. Separate process: closing it never touches recd (it never sends `Quit`), and background work is EcoQoS and paused while recd records.
- Left for later: a native player fallback for PCs where WebView2 can't play HEVC/AV1 (D9 risk; it plays here), the HDR export tone-map (reuse `convert.hlsl`), a microphone device picker, profile create/delete from the app, unstarring from the quick panel, a library view of non-rec videos. M4 (installer) bundles `rec-ui.exe` next to `recd.exe`.

**Things learned**
- Video:
  - The yellow WGC border turns off without any access request for this unpackaged app (Win11 25H2).
  - `GraphicsCaptureItem.Closed` didn't fire when windows closed, so `rec-video` polls once a second that the source still exists.
  - `MinUpdateInterval` is set to half the frame period; a full period starves high-fps games.
  - The AMD HEVC MFT honours peak-constrained VBR.
  - Fullscreen optimizations make DXGI fullscreen games (flip or blt) capturable; the shell then reports "D3D fullscreen". True exclusive fullscreen (reproduce with `__COMPAT_LAYER=DisableDXMaximizedWindowedMode` and the `fake_game` example) still delivers frames, at a reduced rate, but black apart from the cursor. `recd/src/watch.rs` turns that into a "switch to borderless" hint.
  - `StartCapture` can fail with E_INVALIDARG while a game window is switching to fullscreen; recd retries after 2/5/10/30 s and only toasts from the second failure.
  - Restarting the video pipeline (stop + start, AMD HEVC) takes ~220 ms. A clip never spans a restart: it is cut to one segment, and audio is relabelled with that segment's number (rec-mux rejects mixed segments).
  - A real device loss needs a driver reset (TDR), so recovery is tested with `REC_DEBUG_VIDEO_FAIL_AFTER=<s>` (debug builds only: the first pipeline fails after that long) and `fake_game --resize-after <s>`. The adapter-change path (hybrid laptops, a window moved to a monitor on another GPU) is only unit-tested: this machine has one GPU.
- HDR and encoders:
  - HDR is detected per display (`IDXGIOutput6` colour space); WGC then delivers FP16 scRGB. `convert.hlsl` makes P010 HDR10 (PQ, BT.2020, clipped at the display peak) or tone-maps to 8-bit BT.709 using the display's SDR white level. HDR10 mastering/CLL metadata is inserted in-band into every keyframe by the pipeline (`sei.rs`, `obu.rs`) and as `mdcv`/`clli` boxes in the MP4, for every backend alike.
  - Only tested without a real HDR display: `record --assume-hdr` and `REC_DEBUG_ASSUME_HDR=1` (debug builds of recd) take the HDR path on an SDR display. The real-display path (`display::tests::primary_display_reports_its_colour`, the Win+Alt+B restart in `recover.rs`) still needs a run with HDR turned on.
  - AMD AV1 (MFT and AMF, RDNA3) needs 64 x 16 sizes: the MFT pads 1080 to 1082 and hangs after one frame at other unaligned sizes; AMF refuses them. Frames are padded and the picture centred (`geometry::place`). `ENCODER_STALL` (3 s without output) turns any future hang into a pipeline restart.
  - AMF (runtime 1.5.2, driver 32.0.31041, RX 7800 XT): creation 7–18 ms, under one frame of latency, `set_bitrate` under 0.1 ms; each input texture comes back by the poll that returns its output. The minimum runtime (1.4.28) is a guess. AV1 has a visible quality floor on noise at low bitrates.
  - GPU tests: `cargo test --release -p rec-video --lib shader -- --ignored` (shader vs reference, ±1 code) and `cargo test --release -p rec-video amf -- --ignored --test-threads=1`.
  - `record` example: `--codec av1`, `--encoder auto|amf|mf`, `--hdr auto|tonemap|off`, `--assume-hdr`.
- Audio:
  - The MF AAC encoder has zero effective priming delay (measured).
  - WASAPI `AUTOCONVERTPCM` handles all format conversion.
  - Process loopback (measured on Win11 26200): must be event-driven, because polling every 40 ms loses ~25% of the audio. The device position is always 0, so continuity is judged from QPC timestamps. It stamps audio ~21 ms later than endpoint loopback; this is corrected in `wasapi.rs`. It delivers ±1 LSB noise while the app is silent, which is treated as silence.
  - windows-rs `PROPVARIANT` frees its contents on drop (`PropVariantClear`). A PROPVARIANT that borrows a blob must be `ManuallyDrop`, or the heap is corrupted.
  - The MF AAC encoder holds 2 input frames before emitting. Repeated silent packets must stop ≥2 frames before audio returns, or the onset clicks (`silent_packet_repeat_is_transparent`, needs ffmpeg).
  - 4 inputs + mix cost ~2.4% of one core with silent-packet repeat (4.6% without).
  - Debug audio: `cargo run --release -p rec-audio --example capture -- --mix pid:N app:Discord.exe loopback 10 out` writes one `.aac` per track.
- Quick panel:
  - Opens in ~30 ms (DirectComposition + Direct2D on WARP). A hardware D3D device left ~19 MB in recd after the first open; WARP leaves ~6 MB plus ~15 parked worker threads. 60 opens: handles flat, no leak.
  - Clip length uses `ReplayBuffer::set_limits`; quality uses `VideoCapture::set_bitrate` (ICodecAPI in the running MFT; the AMD HEVC MFT accepts it in ~16 ms, same segment, IDR cadence intact); audio switches use `AudioCapture::set_muted`. Settings go to the file via `Config::save_keys`, which keeps comments and layout.
  - Turning on an audio source the session has no input for (e.g. the microphone when it was off at start) applies from the next recording: adding a track mid-buffer would break clips. The panel says so.
  - Testing: `REC_DEBUG_CAPTURE_UI=1` makes it visible to screenshots. A real Alt+F7 press (SendInput) is needed for it to get focus; keys can then be posted to the `rec.panel` window. Refusing to open over exclusive fullscreen is untested on a real exclusive game (it relies on the watcher's `Exclusive` verdict).
- Memory: `recd` uses ~127 MB private while recording at 1080p60. About 54 MB of that is driver / Media Foundation pipeline allocations and the rest is the buffer.
- Bench: `rec-bench` was tested end to end only against stand-in `recd` and PresentMon scripts. The first real run (needs admin or the Performance Log Users group) may show PresentMon CLI differences. Rust `Command` children inherit the parent's std handles, so `rec-bench` clears the inherit flag before restarting the user's `recd`; otherwise `rec-bench | tee` never ends.
- AI-ready clips: verified live with `fake_game` under a separate `APPDATA`/`LOCALAPPDATA` (`game_apps = ["fake_game.exe"]`). Sidecar durations match ffprobe, and marker times agree between the sidecar and the journal. Non-zero loudness and the Steam app id are covered only by unit tests: `fake_game` is silent and not a Steam game. TF2's `appmanifest_440.acf` matches the test format.
- AI-ready clips phase B (`rec-library`):
  - Tested on copies of the phase A live-test clips (recd sidecars and journal, journal paths rewritten), one with its sidecar deleted to test journal-only clips, plus a synthetic 60 s 1440p60 HEVC clip. No new recording was needed, so no game lost focus.
  - The MF source reader decodes on the GPU (`MF_SOURCE_READER_D3D_MANAGER`), and `ENABLE_ADVANCED_VIDEO_PROCESSING` converts to RGB32 and scales on the GPU when `MF_MT_FRAME_SIZE` is set. `SetCurrentPosition` past the end fails with `MF_E_INVALID_POSITION`, so seeks are clamped to just before the end. Seeking fragmented MP4 works.
  - Scene cuts compare colour (grid of mean RGB plus a colour histogram), not luma: `fake_game`'s blue and red frames have equal brightness.
  - Real game footage, HDR10, AV1 and non-silent audio are still unverified for analysis; see [docs/ai-clips.md](docs/ai-clips.md#phase-b-as-built).
  - `cargo test -p rec-library decode -- --ignored` with `REC_TEST_CLIP=<clip.mp4>` decodes a real clip. Claude Code: `claude mcp add rec -- rec mcp`.
- Export (`rec-library::export`, M3):
  - Source reader (GPU decode + video processor to NV12, scaled) -> sink writer (AMD H.264 MFT, same D3D11 device manager, textures passed straight through; peak-constrained VBR, peak 1.5x mean, set through `SetInputMediaType` encoding parameters). Fallback copies frames to system memory (`REC_TEST_EXPORT_CPU=1` forces it in tests).
  - Measured on the RX 7800 XT with a 15.7 s 1080p60 HEVC Deadlock clip: 10 MB -> 720p60 4.76 Mbps, 9.66 MB (96.6% of the limit) in 1.6 s (10x realtime); 50 MB -> 1080p60 12.4 Mbps (the 0.1 bpp cap) in 2.8 s; 3 MB -> 540p30; 1.5 MB -> 360p30. System-memory path ~3-7x realtime. Long clips land at 96.6-97.1% of the limit; 2.5 s trims sometimes overshoot on the first try (the IDR at the start) and the retry fixes it.
  - `MF_MPEG4SINK_MOOV_BEFORE_MDAT` fails at `Finalize` (E_ACCESSDENIED) with a URL sink and writes a corrupt file (mdat header overwritten) with a read/write `MFCreateFile` stream, so `export::faststart` moves `moov` itself and shifts `stco`/`co64`.
  - The MP4 source lists audio streams in reverse track order (2 and 3 tracks, fragmented or not, whichever is enabled; stream identifiers are just 1..n), so `export` reverses them to make track 0 the mix. `audio_tracks_keep_their_order` checks it with one tone per track.
  - A stream selected after the first `ReadSample` loses what the reader already read past (2.5 s of audio went missing): select every stream before reading.
  - The MS AAC encoder takes only 96/128/160/192 kbps, so 96 is the floor.
  - HDR10 -> SDR through the video processor (output type tagged BT.709) is too dark and flat on AMD: mean luma 37 vs 68 for the same footage as SDR, lifted blacks. Tested with an ffmpeg PQ conversion of the Deadlock clip, not a real rec HDR clip. Fix: rec-video's tone-map shader on the decoded P010, in a crate both can use. AV1 sources with padded frames (aperture) are untested.
  - Tests: `cargo test --release -p rec-library export -- --ignored --nocapture` (`REC_TEST_CLIP`, `REC_TEST_LIMIT_MB`, `REC_TEST_KEEP`; a synthetic clip without `REC_TEST_CLIP`).
- rec-ui (M3):
  - Build: `npm --prefix ui install`, `npm --prefix ui run build`, then `cargo build -p rec-ui` (it embeds `ui/dist`; `build.rs` writes a placeholder page when it is missing so CI without Node still builds). Live reload: `npm --prefix ui run app:dev` (`TAURI_APP_PATH` points the CLI at `crates/rec-ui`; `--no-default-features` drops the `embed` feature, i.e. `tauri/custom-protocol`). Tauri 2.12 needs Rust 1.90, so `rec-ui` has its own `rust-version`.
  - Tested against the real app through WebView2 DevTools: start it with `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9333`, then drive the page over CDP (`Runtime.evaluate`, `Page.captureScreenshot`; Node 24 has `WebSocket` built in). Computer use can't pick the window (not an installed app). A sandbox `APPDATA`/`LOCALAPPDATA` with `output.directory` pointing at a copy of a clip keeps tests off the real library.
  - Verified that way: library + background analysis thumbnail, HEVC 1080p60 playback in WebView2, the timeline (markers, loud moments, scene cuts, loudness), trim (start snapped 4.5 -> 4.0, output decodes with ffmpeg, sidecar shifted), star, copy (`CF_HDROP` read back), Recycle Bin delete, settings + per-game override/reset keeping comments, start recd from the app + reload, export 10 MB / trimmed 3 MB / cancel. Closing the window ends rec-ui (and its WebView2 processes) while recd keeps running. Not tested: drag-out (needs a real mouse drag), hotkey recording (keys recd holds never reach the page).
  - Svelte 5: `$effect` copying a prop into `bind:value` state broke number fields (edits after a reload were dropped); fields now keep a `draft` (null when not editing) and show `draft ?? value`. Key handlers on `svelte:window` must not assume `e.target` is an element.
  - Release build: `rec-ui.exe` 10.9 MB. Open on the library: 6 MB private for rec-ui, 167 MB with its 6 WebView2 processes, 0.05 s CPU in 20 s idle. All of it goes when the window closes.
  - The app polls recd status every 3 s (15 s hidden) and rescans the library every 10 s while visible; the scan re-parses only changed sidecars.
- Storage quota:
  - Verified with a throwaway `APPDATA`/`LOCALAPPDATA` and 400 MB stand-in files (`SetLength`, never decoded): `rec star`, `rec prune --dry-run`, `rec prune`, and recd's startup prune all deleted only the oldest unstarred rec clip (with its sidecar and `<clip>.rec` folder), keeping starred, read-only and non-rec videos. The prune after a real save runs the same `prune::request` but wasn't run live: a sandboxed recd records whatever game is in the foreground.
  - Starring lives in the sidecar, so a journal-only clip gets a sidecar when starred. `analysis::save` re-reads `starred` from disk because `rec mcp` holds clips in memory for a long time.
  - "Star last clip" (`hotkeys.star_last`, quick-panel row) writes through `store::set_starred` on recd's main thread, only when used: nothing is added while recording. recd doesn't depend on rec-library (MF/D3D11 decoding, WIC), so the sidecar write lives in rec-core and rec-library's `write_sidecar` calls it. rec-core's `serde_json` has `preserve_order`, so a star keeps the file's key order in recd-only builds too. With `output.sidecar = false`, the save thread keeps a layer-0 sidecar (`sidecar::minimal`, the same facts `rec star` builds from the journal) in memory for the last clip, written only when it is starred. Unit-tested only: not run live, for the same reason as the prune after a save.
  - Ideas not done: a low-disk warning, pruning folders the clips used to be saved to, unstarring from the panel.
- Controller combos: an empty XInput slot read costs ~16–20 µs, so empty slots are checked once a second; with a pad, 30 Hz costs ~0.08% of a core. Hardware run: `cargo test --release -p recd poll_cost -- --ignored --nocapture` (`REC_TEST_SECONDS`). `cargo test -p recd shows_the_panel -- --ignored` shows the panel without recd.
- DDA:
  - `AcquireNextFrame` with a timeout holds the D3D11 device lock while waiting (a 50 ms wait dropped 84 of 150 ticks), so DDA polls with no wait every half tick.
  - DDA in FP16 on an SDR display returns sRGB-encoded values, so `--assume-hdr` with DDA is too bright.
  - DISPLAY1 on this machine is a rotated portrait monitor; `auto` keeps WGC there. 0x887A0026 is `DXGI_ERROR_ACCESS_LOST`.
  - No cursor with DDA; windows covering the game are recorded.
- GPU priority: unelevated, every priority setting is accepted except hard realtime. With hardware GPU scheduling on (RDNA3) nothing preempts a running game command buffer, so priority changed nothing; drops start once the game's frames take over ~40 ms. Untested with HAGS off (needs a reboot). Test: `gpu_stress --iterations 32000 --draws 1` + `record --window-title "rec gpu stress" --gpu-priority adaptive`.
- Profiles: driving the panel from a test needs a real Alt+F7 then `keybd_event` arrows (`FindWindow("rec.panel")` returned 0). The line editor keeps empty `[section]` headers, so templates use real headers with commented examples. A profile doesn't mark its exe as a game; that still needs `game_apps`.
- Agent worktrees may start from `main`, not `m2`: check the base first.
- Logs: `%LOCALAPPDATA%\rec\recd.log`. Config: `%APPDATA%\rec\config.toml`.
- Testing: set `REC_DEBUG_CAPTURE_UI=1` to make toasts visible in screenshots.

**Conventions**
- Timestamps are 100 ns ticks (`rec_core::time::Hns`) on the QPC clock.
- Encoder output must have no B-frames and an IDR every 1 s (the replay buffer relies on it).
- No GPL dependencies, no injection, no `WH_KEYBOARD_LL`.
- Check before merging: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- Hardware tests are `#[ignore]`d. Run them with `cargo test --release -p rec-video --test hardware -- --ignored`.
