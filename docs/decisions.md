# Decisions

Short records of the choices that shape rec. Each has the reason, so it can be revisited when the facts change. Background evidence is in [research.md](research.md).

## D1. Rust for everything (service, CLI, UI backend)

**Performance:** Rust and C++ are a tie at runtime. Both compile to native code through LLVM/MSVC-class optimizers, have no garbage collector, and call the same Windows APIs. The expensive work happens on the GPU (capture copy, colour conversion, the hardware encoder), so the host language barely affects game FPS.

**Why Rust wins on everything else:**
- **Reliability of an always-on process.** A leak, crash or data race in the recorder means "replay silently turned off", which is the #1 complaint about every competitor. Rust removes most of those failure classes at compile time.
- **Threading.** Capture, encode, audio and save threads share buffers safely.
- **Tooling.** [windows-rs](https://github.com/microsoft/windows-rs) is Microsoft's official binding to all Win32/WinRT/COM APIs. Cargo gives reproducible builds with no CMake or vcpkg.
- **One language end to end.** The settings UI backend (Tauri, D9) reuses `rec-core` directly.

**Cost:** The vendor encoder SDKs (NVENC, AMF, oneVPL) are C APIs. We write thin FFI wrappers for them.

## D2. Windows 10 22H2 and Windows 11, x64

Features are checked at runtime, never by OS version string alone:

| Feature | Available on | Fallback |
|---|---|---|
| Windows.Graphics.Capture (WGC) | Win10 1903+ | Desktop Duplication (DDA) |
| Borderless WGC (`IsBorderRequired = false`) | Win11 (build 20348+) | Win10 uses DDA display capture cropped to the game window, so no yellow border is shown around the game |
| `MinUpdateInterval` (cap the capture rate) | Win11 24H2 (26100+) | Drop surplus frames on our fixed encode tick |
| Per-process audio loopback | Documented for 20348+; works on Win10 2004+ (build 19041), which is what rec checks | Endpoint loopback (all system audio) on one track |
| `WDA_EXCLUDEFROMCAPTURE` (hide our overlay from clips) | Win10 2004+ | n/a |
| Media Foundation | All, **except "N" editions without the Media Feature Pack** | Detect it and tell the user how to install the pack |

## D3. No code injection into games

Capture uses only OS compositor APIs (WGC, DDA). We do no DLL injection, no graphics-API hooks and no low-level keyboard hooks, and open no game process handles beyond `PROCESS_QUERY_LIMITED_INFORMATION`.
- **Why:** this keeps rec safe under Vanguard, EAC, BattlEye, FACEIT and Ricochet. It also avoids the black-screen and crash class of bugs that hook-based capture has.
- **Cost:** true exclusive-fullscreen games (rare on Win10/11, because Fullscreen Optimizations turn most into flip-model borderless) cannot be captured. We detect it and tell the user to switch to borderless, never failing silently.
- **Later:** a separate, opt-in hook module for non-anti-cheat games may come later. It will never be on by default.

## D4. No FFmpeg: OS and driver encoders plus our own muxer

**Video encode:**
1. The first backend is **Media Foundation hardware encoders**. They ship with Windows and work on NVIDIA, AMD and Intel with zero bundled dependencies.
2. **Native NVENC / AMF / oneVPL** backends follow, loaded at runtime from the GPU driver's own DLLs. They give finer rate control, AV1 on drivers whose MF encoder lacks it, and handling of session limits.
3. The software fallback is Microsoft's built-in H.264 encoder, opt-in only.

**Audio encode:** Media Foundation AAC, built into Windows. Input is always 48 kHz 16-bit stereo, produced by WASAPI's own `AUTOCONVERTPCM` conversion, so a device change never changes the track format.

**Audio tracks:** each source gets its own track: the game, voice chat apps and "other apps" through process loopback (by process tree, include or exclude), and the microphone. A mixed track (what a viewer should hear) comes first, so players that only play one track play the mix. We mix ourselves, on the QPC sample grid every input's timeline already uses, rather than relying on the OS. A silent track repeats one cached silent AAC packet instead of encoding; it stays a few frames behind the silence so the packets that overlap returning audio still come from the encoder.

**Muxing:** our own fragmented-MP4 writer (and later MKV). It is a few hundred lines with full control over crash safety, multiple audio tracks and segment changes.

**Why:**
- No 20–40 MB of FFmpeg DLLs.
- No painful FFmpeg build for Rust on Windows.
- No LGPL relinking obligations.
- No GPL x264.
- We only need encoders the OS and driver already provide, plus a muxer.

**Cost:** we must test our MP4 output against players, editors, browsers and Discord.

## D5. Replay buffer holds encoded packets in RAM

The buffer stores compressed packets, never raw frames. It always starts on a keyframe, evicts whole GOPs (keyframe to keyframe), and is capped by both duration and bytes. Saving is a remux with no re-encode.
- **Implemented:** `crates/rec-core/src/replay.rs`.
- **Memory:** 60 s at 50 Mbit/s ≈ 375 MB.
- **Later:** an opt-in disk segment ring for long or high-bitrate buffers.

**Encoder rules that the buffer relies on:**
- No B-frames.
- An IDR keyframe every 1 s.
- Capped VBR.

## D6. Process model

| Process | Role | Footprint goal |
|---|---|---|
| `recd` | Always on. Pure Win32: no UI framework, no web view. Handles capture, encode, buffer, hotkeys, the tray icon, the in-game overlay and toasts (DirectComposition), and serves the IPC pipe. | < 15 MB private memory + buffer; ~0% CPU with no game running |
| `rec` | CLI over the IPC pipe (`rec save 30`, `rec status`). Used by scripts, Stream Deck and macro tools. | Runs and exits |
| `rec-ui` | Settings, clip library and trimmer. Launched on demand and **fully exits when closed**. | Irrelevant while gaming |

Closing the UI can never stop the recording.

## D7. Hotkeys

- **Defaults:** `Alt+F8` saves a clip, `Alt+F7` opens the quick panel. Neither collides with the NVIDIA, AMD, Xbox Game Bar or Steam defaults. Known conflicts are flagged in `rec-core::hotkey`.
- **Mechanism:** `RegisterHotKey` for single actions. Raw Input (`RIDEV_INPUTSINK`) for chords it cannot claim. XInput polling, only while a game is running, for controller combos. **No `WH_KEYBOARD_LL`**: it adds input latency and looks like a keylogger to anti-cheat and antivirus software.
- **Validation:** letters and digits need a modifier. F12 is rejected because Windows reserves it.

## D8. In-game overlay without injection

- **Overlay:** the quick panel is a topmost DirectComposition window drawn with Direct2D and DirectWrite (on WARP, so its drawing never touches the game's GPU queue). It is marked `WDA_EXCLUDEFROMCAPTURE` so it never shows up in clips. Toasts stay small click-through, non-activating layered windows.
- **Focus:** the panel takes focus while open. Without injection or a keyboard hook, that is the only way to read its keys without the game also acting on them (Esc would open the game's menu). NVIDIA, AMD and Xbox Game Bar overlays do the same. Closing it (Esc, the hotkey, or clicking elsewhere) gives focus back to the game.
- **Limits:** it works over borderless and Fullscreen-Optimized games. Over true exclusive fullscreen (detected from black capture, see `recd/src/watch.rs`) the panel does not open, because taking focus would minimize the game; rec falls back to sound cues.
- **Lifetime:** created on open and released on close, so it costs nothing while gaming and only breaks the game's independent flip while the user is in it.
- **Changes never cost the buffer:** clip length resizes the buffer in place, quality changes the encoder's bitrate in place (the clip stays one segment), audio switches mute the input. All are saved to `config.toml` without rewriting the rest of the file.

## D9. Settings & library UI: Tauri 2 + Svelte 5 + TypeScript

**Why:**
- **No cost while gaming.** The UI runs only when opened, so its framework costs nothing during play.
- **Shared code.** Tauri's backend is Rust and links `rec-core` directly: the same config types and the same IPC client.
- **Runtime already present.** WebView2 is preinstalled on Windows 11 and on almost all Windows 10 machines. Tauri's installer bootstraps it where missing.
- **Fastest path to a polished UI.** It is the quickest route to a polished clip library, trimmer and settings screens with video playback.
- **Small framework.** Svelte compiles away to a tiny runtime.

**Rejected:**
- **WinUI 3:** a weak Rust story, and the Windows App SDK runtime must be installed.
- **egui / Slint:** fine for settings but weak for a media library with playback.
- **Pure Win32:** far too slow to build a good library UI.

**Risk to verify early:** HEVC playback inside WebView2 depends on hardware decode support. The fallback is a native Media Foundation player window opened from the UI.

## D10. License: MIT OR Apache-2.0

Free and open source, using the standard Rust dual license.
- **No GPL dependencies:** no x264, and no copying code from OBS (GPLv2). We read OBS and Sunshine to learn, then write our own implementation.
- **Network:** no telemetry, no accounts, no network access unless the user turns on update checks or sharing.

## D11. Performance is a feature, measured every release

**Budgets:**
- **No game running:** < 0.2% CPU, < 15 MB private memory.
- **Recording 1440p60 HEVC:** ≤ 1–2% average-FPS loss and ≤ 2% 1%-low loss, < 1% CPU.
- **On save:** no frame-time spike.

**Measurement:** PresentMon/CapFrameX on NVIDIA, AMD, Intel and a hybrid laptop. Results are published in the README.

## D12. HDR: record what the display shows, as HDR10

When Windows HDR is on for the captured display, rec captures the compositor's FP16 scRGB picture and records **HDR10**: 10-bit HEVC Main10 or AV1, BT.2020 primaries, PQ, limited range. `hdr = "tonemap"` records SDR from the same capture instead; `hdr = "off"` captures 8-bit as Windows presents it (highlights clip). SDR displays are unaffected.
- **Our own shaders, not the video processor:** what D3D11 video processors do with FP16 input differs between drivers. Two small pixel shaders (compiled at start by the OS's `d3dcompiler_47.dll`) scale, letterbox, convert and write the P010 or NV12 planes directly, and a GPU test checks them against a CPU reference to within one code value.
- **Honest metadata:** the mastering display is the user's monitor as DXGI reports it, and the picture is clipped at its peak, so MaxCLL is its peak. A clip never claims more than the display could show. Unknown panels get the conventional BT.2020 / 1000 cd/m².
- **Metadata everywhere a player looks:** VUI / sequence-header colour from the encoder, the MP4 `colr` / `mdcv` / `clli` boxes, and the static metadata in-band in every keyframe (HEVC SEI, AV1 metadata OBUs), inserted by us so it is the same for every encoder and survives cutting and remuxing. The "washed out" HDR clips users complain about come from metadata missing in one of these places.
- **Tone-map:** SDR white (Windows' "SDR content brightness") maps to 100%; above 85% of it highlights roll off exponentially per pixel on the brightest channel, so hues hold and nothing clips hard. SDR white ends up about 5% dimmer in linear light (2.5% in signal) than on an SDR capture, the price of keeping highlight detail.
- **Fallbacks, never a failed recording:** H.264 has no 10-bit hardware profile, so H.264 tone-maps; an encoder without 10-bit falls back to the tone-map with a logged note.
- **Switching HDR mid-game** restarts only the video pipeline in the new mode (see `recd/src/recover.rs`), like a resolution change.
