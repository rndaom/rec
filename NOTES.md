# Working notes

Handoff file so any session can pick up quickly. Keep it short and current.

**Goal:** rec, a lightweight, open-source, anti-cheat-safe instant-replay clipper for Windows 10/11 (see README).

**Read first:** [docs/decisions.md](docs/decisions.md) (the why), [docs/roadmap.md](docs/roadmap.md) (the what), [docs/research.md](docs/research.md) (the evidence).

**Status (2026-10-12):** M1 done, verified on a live game.

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
1. Benchmark harness, to lock in the performance budget early (harness done; the first published run is still to do)
2. Per-process audio tracks
3. Black-frame / exclusive-fullscreen detection
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
- Audio:
  - The MF AAC encoder has zero effective priming delay (measured).
  - WASAPI `AUTOCONVERTPCM` handles all format conversion.
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
