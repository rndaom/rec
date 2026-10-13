# Working notes

Handoff file so any session can pick up quickly. Keep it short and current.

**Goal:** rec, a lightweight, open-source, anti-cheat-safe instant-replay clipper for Windows 10/11 (see README).

**Read first:** [docs/decisions.md](docs/decisions.md) (the why), [docs/roadmap.md](docs/roadmap.md) (the what), [docs/research.md](docs/research.md) (the evidence).

**Status (2026-10-12):** M0 done.
- `rec-core` (config, hotkey, ipc, replay, time) is implemented and tested.
- The `rec` CLI client is complete.
- `recd` is a stub that only handles config.

**Next step:** start M1 with `recd`. Order:
1. Windows subsystem + single instance + tray
2. Named-pipe IPC server
3. WGC capture → D3D11 video processor NV12 → Media Foundation HW encoder → replay buffer
4. Save hotkey → fMP4 writer
5. Audio

**Conventions**
- Timestamps are 100 ns ticks (`rec_core::time::Hns`) on the QPC clock.
- Encoder output must have no B-frames and an IDR every 1 s (the replay buffer relies on it).
- No GPL dependencies, no injection, no `WH_KEYBOARD_LL`.
- Check before merging: `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
