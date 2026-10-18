# Working notes

Handoff file so any session can pick up quickly. Keep it short and current.

**Goal:** rec, a lightweight, open-source, anti-cheat-safe instant-replay clipper for Windows 10/11 (see README).

**Read first:** [docs/decisions.md](docs/decisions.md) (the why), [docs/roadmap.md](docs/roadmap.md) (the what), [docs/research.md](docs/research.md) (the evidence).

**Status (2026-10-18):** M1 done, verified on a live game. M2 in progress on branch `m2`: per-process audio, exclusive-fullscreen detection, pipeline recovery, the quick panel, HDR, AV1, the AMD AMF backend, AI-ready clips phases A and B, and the storage quota done.

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

**Next step:** M2. Suggested order:
1. ~~Benchmark harness~~ (harness done; the first published run on real hardware is still to do)
2. ~~Per-process audio tracks~~ (done)
3. ~~Black-frame / exclusive-fullscreen detection~~ (done)
4. ~~Pipeline recovery~~ (done: failures, resolution changes and GPU moves restart only the video pipeline as a new `segment`; see `recd/src/recover.rs`)
5. ~~Quick-panel overlay~~ (done: `recd/src/panel.rs` model, `overlay.rs` window; Alt+F7 or the tray)
6. ~~HDR~~ (done: HDR10 or tone-map, decision D12)
7. ~~Native encoders + AV1~~ (AV1 and AMD AMF done; NVENC and oneVPL need NVIDIA / Intel hardware)
8. ~~AI-ready clips phase A~~ (done: `<clip>.rec.json` sidecar, marker hotkey and `rec mark`, session journal, loudness curves; see [docs/ai-clips.md](docs/ai-clips.md)). The marker hotkey has no default: set `hotkeys.add_marker`. Phase B is done too: `rec digest`, `rec analyze`, `rec mcp` and layer 2 analysis (thumbnail, scene cuts, brightness, motion, loud moments). Phases C and D (transcripts, game sources) are proposals.
9. ~~Storage quota with auto-prune~~ (done: `output.quota_gb`, `rec star` / `unstar` / `prune`, decision D13; `recd/src/prune.rs`, `rec-core/src/store.rs`)
10. Remaining M2 items: per-game profiles, extra clip-length hotkeys, controller combo, Win10 DDA fallback, adaptive GPU priority

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
- Storage quota:
  - Verified with a throwaway `APPDATA`/`LOCALAPPDATA` and 400 MB stand-in files (`SetLength`, never decoded): `rec star`, `rec prune --dry-run`, `rec prune`, and recd's startup prune all deleted only the oldest unstarred rec clip (with its sidecar and `<clip>.rec` folder), keeping starred, read-only and non-rec videos. The prune after a real save runs the same `prune::request` but wasn't run live: a sandboxed recd records whatever game is in the foreground.
  - Starring lives in the sidecar, so a journal-only clip gets a sidecar when starred. `analysis::save` re-reads `starred` from disk because `rec mcp` holds clips in memory for a long time.
  - Ideas not done: a star button in the quick panel or a "star last clip" hotkey, a low-disk warning, pruning folders the clips used to be saved to.
- Logs: `%LOCALAPPDATA%\rec\recd.log`. Config: `%APPDATA%\rec\config.toml`.
- Testing: set `REC_DEBUG_CAPTURE_UI=1` to make toasts visible in screenshots.

**Conventions**
- Timestamps are 100 ns ticks (`rec_core::time::Hns`) on the QPC clock.
- Encoder output must have no B-frames and an IDR every 1 s (the replay buffer relies on it).
- No GPL dependencies, no injection, no `WH_KEYBOARD_LL`.
- Check before merging: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- Hardware tests are `#[ignore]`d. Run them with `cargo test --release -p rec-video --test hardware -- --ignored`.
