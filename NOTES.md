# Working notes

Handoff file so any session can pick up quickly. Keep it short and current.

**Goal:** rec, a lightweight, open-source, anti-cheat-safe instant-replay clipper for Windows 10/11 (see README).

**Read first:** [docs/decisions.md](docs/decisions.md) (the why), [docs/roadmap.md](docs/roadmap.md) (the what), [docs/research.md](docs/research.md) (the evidence).

**Status (2026-10-12):** M1 done, verified on a live game. M2 in progress on branch `m2`: per-process audio and exclusive-fullscreen detection done.

| Crate | Role |
|---|---|
| `rec-core` | Config, hotkeys, IPC, replay buffer, media types, QPC clock |
| `rec-video` | WGC → D3D11 NV12 → MF hardware encoder |
| `rec-audio` | WASAPI → AAC |
| `rec-mux` | Fragmented MP4 |
| `recd` | Tray service |
| `rec-cli` | `rec` |
| `rec-bench` | Benchmark harness: PresentMon off/on runs, `recd` idle cost (see `bench/README.md`) |

**Next step:** M2. Suggested order:
1. ~~Benchmark harness~~ (harness done; the first published run on real hardware is still to do)
2. ~~Per-process audio tracks~~ (done)
3. ~~Black-frame / exclusive-fullscreen detection~~ (done)
4. Pipeline recovery (device lost, resolution change), using new `segment` numbers
5. Quick-panel overlay
6. HDR
7. Native encoders + AV1

**Things learned**
- Video:
  - The yellow WGC border turns off without any access request for this unpackaged app (Win11 25H2).
  - `GraphicsCaptureItem.Closed` didn't fire when windows closed, so `rec-video` polls once a second that the source still exists.
  - `MinUpdateInterval` is set to half the frame period; a full period starves high-fps games.
  - The AMD HEVC MFT honours peak-constrained VBR.
  - Fullscreen optimizations make DXGI fullscreen games (flip or blt) capturable; the shell then reports "D3D fullscreen". True exclusive fullscreen (reproduce with `__COMPAT_LAYER=DisableDXMaximizedWindowedMode` and the `fake_game` example) still delivers frames, at a reduced rate, but black apart from the cursor. `recd/src/watch.rs` turns that into a "switch to borderless" hint.
  - `StartCapture` can fail with E_INVALIDARG while a game window is switching to fullscreen; recd retries after 2/5/10/30 s and only toasts from the second failure.
- Audio:
  - The MF AAC encoder has zero effective priming delay (measured).
  - WASAPI `AUTOCONVERTPCM` handles all format conversion.
  - Process loopback (measured on Win11 26200): must be event-driven, because polling every 40 ms loses ~25% of the audio. The device position is always 0, so continuity is judged from QPC timestamps. It stamps audio ~21 ms later than endpoint loopback; this is corrected in `wasapi.rs`. It delivers ±1 LSB noise while the app is silent, which is treated as silence.
  - windows-rs `PROPVARIANT` frees its contents on drop (`PropVariantClear`). A PROPVARIANT that borrows a blob must be `ManuallyDrop`, or the heap is corrupted.
  - The MF AAC encoder holds 2 input frames before emitting. Repeated silent packets must stop ≥2 frames before audio returns, or the onset clicks (`silent_packet_repeat_is_transparent`, needs ffmpeg).
  - 4 inputs + mix cost ~2.4% of one core with silent-packet repeat (4.6% without).
  - Debug audio: `cargo run --release -p rec-audio --example capture -- --mix pid:N app:Discord.exe loopback 10 out` writes one `.aac` per track.
- Memory: `recd` uses ~127 MB private while recording at 1080p60. About 54 MB of that is driver / Media Foundation pipeline allocations and the rest is the buffer.
- Bench: `rec-bench` was tested end to end only against stand-in `recd` and PresentMon scripts. The first real run (needs admin or the Performance Log Users group) may show PresentMon CLI differences. Rust `Command` children inherit the parent's std handles, so `rec-bench` clears the inherit flag before restarting the user's `recd`; otherwise `rec-bench | tee` never ends.
- Logs: `%LOCALAPPDATA%\rec\recd.log`. Config: `%APPDATA%\rec\config.toml`.
- Testing: set `REC_DEBUG_CAPTURE_UI=1` to make toasts visible in screenshots.

**Conventions**
- Timestamps are 100 ns ticks (`rec_core::time::Hns`) on the QPC clock.
- Encoder output must have no B-frames and an IDR every 1 s (the replay buffer relies on it).
- No GPL dependencies, no injection, no `WH_KEYBOARD_LL`.
- Check before merging: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- Hardware tests are `#[ignore]`d. Run them with `cargo test --release -p rec-video --test hardware -- --ignored`.
