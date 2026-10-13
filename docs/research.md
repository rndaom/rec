# rec: Competitive & Technical Research

_Compiled 2026-10-12. Sources: official docs/changelogs, vendor support pages, Steam/NVIDIA/AMD/OBS/Discord forums, GitHub, Trustpilot. Reddit could not be fetched directly, so community sentiment comes from the other forums. Most published FPS numbers come from vendors or competitors; treat them as directional, not proof._

---

## 1. TL;DR

- **The opportunity is real.** Nobody ships a *vendor-neutral, ShadowPlay-light, local-first* clipper. NVIDIA App is light but NVIDIA-only. AMD's is buggy and AMD-only. Intel has nothing. Game Bar is weak and costly (~10% FPS in one test). Medal/Outplayed are Electron/Overwolf-heavy, ad-supported, account- and cloud-pushing. OBS is light but complex and not built for clipping. Open-source attempts are early (0–6 stars, mostly CLI, mostly NVIDIA-only).
- **The #1 user complaint across every product is that instant replay silently turns off.** The second is performance and bloat. Then capture failures (black screen, anti-cheat), audio (no separate tracks, Discord missing, desync), storage chaos, and washed-out HDR.
- **Technical recipe for "near-zero impact":**
  - Capture with Windows.Graphics.Capture (WGC) or DXGI Desktop Duplication. **No injection.**
  - Keep frames on the GPU and convert BGRA→NV12 on the GPU.
  - Encode on the GPU's dedicated hardware encoder (NVENC/AMF/QSV).
  - Keep the last N seconds as **already-compressed packets** in a byte-capped RAM ring.
  - On hotkey, **remux** (no re-encode) to a crash-safe MP4/MKV on a low-priority thread.
  - Run a tiny pure-Win32 tray service; the settings/library UI is a separate process launched only on demand.

---

## 2. Competitor landscape

| Product | Price | GPU | Replay length | Codecs | Separate / per-app audio | Auto-clip | Editor | Share | Known overhead / notes |
|---|---|---|---|---|---|---|---|---|---|
| **NVIDIA App** (ShadowPlay) | Free | NVIDIA only | ~30 s–20 min | H.264, HEVC, AV1 (RTX 40+) | Game + mic (no per-app) | "Highlights" in a few games | Basic | No | Lightest; 240 fps capture on RTX 40/50; Alt+Z overlay. Replay often auto-disables (DRM/desktop capture). |
| **AMD Adrenalin** (ReLive) | Free | Radeon only | ≤20 min (15 s steps) | H.264, HEVC, AV1 (RX 7000+) | Basic | No | No | No | Recurring driver bugs: flicker, replay turning itself off, stutter reports |
| **Intel** | — | — | — | — | — | — | — | — | Capture effectively gone from current Intel software |
| **Xbox Game Bar** | Free | Any | 30 s–10 min | H.264, 30/60 fps only | No | No | No | Xbox app | ~10.5% FPS loss in one (competitor) test; fails in exclusive FS; silent failures |
| **Steam Game Recording** | Free | Any | Up to 120 min, per-game | H.264 (≤1920×1200), HEVC | "Game + selected programs" | Markers (achievements, CS2/Dota events) | **Timeline editor** | Steam friends / MP4 export | Great "find it afterwards" UX; HDR washed out; reports of undeletable storage bloat |
| **Medal.tv** | Free (ads, watermark, 1080p60 cap) / Premium | Any | 15 s–10 min + session | — | **Game / mic / Discord tracks** | Kill/ace detection (some games), voice-trigger clipping | Web editor (captions, slow-mo) | Cloud links, social feed | Electron; ~7% FPS loss in one test; uninstall/persistence complaints; clip archive used to train AI (General Intuition) |
| **Outplayed** (Overwolf) | Free w/ ads / Premium | Any | Highlights / match / session | — | — | Event-based (Overwolf game events), 5000+ games | Trim, "Supercut" | Yes | Requires Overwolf client; post-game pop-ups; records everything by default |
| **Insights Capture** | Free | Any | Full match + event timeline | — | — | Event markers, 10k+ games (claimed) | Trim/merge/caption, team VOD review | Non-expiring links | Standalone; no independent perf data |
| **Discord Clips** | Nitro / early access | Any | 30 s / 1 / 2 min | MP4/GIF | No | No | No | Discord | Discord warns about perf on low-end PCs |
| **OBS Replay Buffer** | Free, OSS | Any | User-set (RAM) | All | Multi-track (plugin for per-app) | No | No | No | ~2% FPS with NVENC; ~15% with x264. Complex setup; save fails after sleep/hibernate (open issue) |
| **Allstar / Eklipse** | Freemium / paid | Server/cloud | — | — | — | Server-side / AI on VODs | AI crop to vertical | Yes | Zero local cost, but limited games, cloud-only |
| **SteelSeries GG Moments** | Free | Any | — | — | — | HUD-reading AI, narrow list | Quick trim | Yes | No Windows HDR support |
| **Razer Cortex Game Clips** | Free? | Any? | 15 s–5 min | — | — | Auto-clip in ~9 games (2026) | — | — | New entrant (Dec 2025) |
| **OSS: rbuf / OpenReplay / trix** | Free | Mostly NVIDIA / any | N sec | H.264/HEVC/AV1 | rbuf: per-process tracks | No | trix: stream-copy trim | No | Very early, CLI or rough UI. Good code to study. |

**Takeaways**
- **Hardware is solved; trust and UX are not.** The light tools (NVIDIA, OBS) are hostile to beginners or vendor-locked. The friendly tools (Medal, Outplayed) are heavy and monetize you.
- **Nobody publishes reproducible performance numbers.** Doing so would differentiate us immediately.

---

## 3. What users complain about (ranked)

1. **Replay silently turns off.** Seen on NVIDIA (DRM content, alt-tab, desktop capture, OBS open, driver updates), AMD (won't stay enabled), and OBS (save fails after sleep). Users discover it only *after* the epic moment is lost.
2. **Performance & bloat.** FPS drops, hitches every few seconds (AMD), heavy CPU and fans (Medal), slow startup, Electron/Overwolf running in the background. NVIDIA's measured 2–15% hit came from its *filters/overlay*, not replay, but the whole app got the blame.
3. **Bloat, ads, forced accounts, cloud upload.** Medal relaunches after uninstall and caps the free tier at 1080p60 plus a watermark. Auto-clip and auto-upload are on by default. Account recovery is a nightmare. Outplayed pops up after every match.
4. **Capture failures.**
   - Black screen (OBS game capture under anti-cheat).
   - Exclusive fullscreen not captured (Game Bar).
   - Borderless or admin-mode games not recorded (AMD).
   - Workaround culture: "run OBS as admin and start the buffer manually every time."
5. **Audio.**
   - No per-app separate tracks; users want to drop Discord or the mic per clip.
   - Game or Discord audio disappearing after a headset change.
   - Audio starting 5–15 s late (Game Bar).
   - Steam only captures the default device.
6. **Storage chaos.** Steam: 200 GB+ of footage users couldn't delete. Medal: clips "disappearing", 659 GB folders. Save button greyed out after changing the folder (NVIDIA).
7. **HDR clips washed out or blown out** (Steam, AMD). Users hand-patch metadata with ffmpeg.
8. **Wrong monitor or extra content captured.** Desktop capture records the second monitor, notifications, DMs.
9. **Sharing friction.** HEVC/AV1 clips that won't play inline on Discord or in browsers. Discord file-size limits.
10. **Laptops with hybrid graphics.** Choppy recordings unless the user forces dGPU-only mode.

**Top explicit wishes:**
- Visible buffer status and auto re-arm.
- Per-app audio tracks.
- HDR tone-mapping.
- Game-only capture by default.
- Disk-backed buffer option.
- Save-and-flush so back-to-back clips don't overlap.
- Fixed-length save with no trimming needed.
- Storage cap with auto-prune.
- Local-only, no account.
- Discord-ready export.
- A clean uninstall.
- Works under anti-cheat without admin rights.

---

## 4. Features worth taking (and from whom)

| Feature | Best current example |
|---|---|
| GPU-direct hardware encode, near-zero overhead | NVIDIA App |
| AV1/HEVC/H.264 choice, high fps (up to 240) capture | NVIDIA App |
| Long buffer with **per-game** length & quality | Steam |
| Timeline with markers → "clip it afterwards" | Steam, Insights |
| Marker hotkey during play | Steam (Ctrl+F12) |
| Separate game / mic / voice-chat tracks | Medal |
| Per-app audio selection | Steam, rbuf |
| Voice-triggered clipping ("clip that") | Medal, Eklipse |
| Event auto-clipping | Insights, Outplayed (but they're heavy) |
| Lossless stream-copy trim | trix |
| Live "buffer size / RAM used" estimate | OpenReplay |
| Overlay process separate from capture (closing UI can't kill the buffer) | OpenReplay |
| Scriptable control (CLI / IPC: `save`, `mark`, `stop`) | rbuf, gpu-screen-recorder |
| Post-save script hook | gpu-screen-recorder |
| Vertical (9:16) crop export for Shorts/TikTok | Eklipse |
| Desktop-capture mode for undetected apps | SteelSeries GG |
| Free tier with no watermark, no ads | Insights |

---

## 5. Gaps you haven't mentioned (recommended additions)

**Reliability & trust**
- **Buffer health indicator.** A tray icon state (green, amber, red) plus a toast that says *why* the buffer stopped. Re-arm automatically after alt-tab, resolution change, driver update, or sleep.
- **"Clip saved" feedback that works in exclusive fullscreen.** An audio cue, because no overlay can draw there without injection.
- **Save-and-flush and pre/post-roll.** For example, 30 s before plus 5 s after the press. Two presses in a row should not produce overlapping duplicate clips (or offer a "merge" option).
- **Multiple save hotkeys for different durations.** For example, 15 s, 60 s, and the full buffer.
- **Crash-safe files.** Use fragmented MP4 or MKV, so a crash mid-save still leaves a playable file.

**Audio**
- Separate tracks for game, voice chat (Discord, TeamSpeak), mic, and "everything else".
- Pick per-app sources (Windows process loopback).
- Mic: push-to-talk awareness, optional light noise suppression, mute toggle in the overlay.
- Export with a choice: single mixed track (for sharing) or multi-track master (for editing).

**Privacy**
- Capture **the game window only** by default, so no notifications, DMs, or second monitor end up in clips.
- Automatically exclude or pause on a sensitive-app list (password managers, banking, private browsing).
- Use `WDA_EXCLUDEFROMCAPTURE` so our own overlay and toasts never appear in clips.
- No account, no telemetry by default (opt-in only). Never upload unless the user asks.

**Storage & organization**
- One folder per game. Filename template, for example `{game}_{date}_{time}_{duration}s`.
- Multiple destinations: local, NAS, or a OneDrive/Dropbox/Drive *folder* (sync handled by the user's client).
- Storage quota with oldest-first auto-prune. Low-disk warnings. "Starred" clips are never pruned.
- Delete that actually deletes, or sends to the Recycle Bin.

**Sharing**
- One-click "Discord-ready" export: H.264 MP4 under 10, 25, 50, or 500 MB, automatically re-encoded from HEVC/AV1.
- Copy the file to the clipboard for paste into Discord. Drag-and-drop out of the library.
- Vertical crop preset. GIF export for short clips.
- Optional integrations later: Discord webhook, YouTube, Streamable.

**Device & context**
- **Handheld PCs and laptops** (ROG Ally, Legion Go, Steam Deck on Windows):
  - Battery-aware mode: lower fps, or pause when on battery.
  - Controller combo to clip (e.g., hold View+Menu).
- **Hybrid GPUs.** Capture and encode on the adapter that owns the display, and show which encoder is in use.
- **HDR.** Correct HDR10 tagging, *or* automatic HDR→SDR tone-map for sharing.
- **Multi-monitor.** Follow the game's monitor automatically. Pick a monitor explicitly when in desktop mode.
- **Coexistence with other tools.** NVIDIA, AMD, Steam, and OBS have conflicting default hotkeys. GeForce cards are limited to 8 NVENC sessions. Detect both and warn.

**Power-user hooks**
- Screenshot hotkey. Marker/bookmark hotkey.
- Local IPC/CLI. An Elgato Stream Deck plugin. A post-save script hook.
- Per-game profiles, auto-created when a game is first detected.

**Auto-clipping (later phase, local and private)**
- Use **official game APIs** that are allowed and run locally:
  - CS2 and Dota 2 Game State Integration.
  - League of Legends Live Client Data API (localhost).
- Add a local voice trigger ("clip that"). Local auto-clipping that respects privacy is unclaimed territory.

**Product hygiene**
- **Distribution.**
  - Code-signed binaries (SmartScreen and anti-cheat reputation).
  - No admin rights needed.
  - Clean uninstall.
  - Updates never applied while a game is running.
  - Portable mode.
- **Accessibility.** Remappable everything, keyboard-only and screen-reader-friendly UI, localization.
- **Licensing.**
  - Use FFmpeg/libav as an LGPL dynamic-link build for muxing.
  - Avoid GPL x264 if the app will be closed-source.
  - Lean on hardware encoders and Windows' built-in AAC encoder.
  - Not legal advice; check before shipping.

---

## 6. Recommended technical architecture

### 6.1 Process model
```
clipd.exe   (always-on, pure Win32 tray app, no UI framework)
  ├─ capture (WGC / DDA) → GPU convert → HW encoder
  ├─ audio (WASAPI process/endpoint loopback + mic) → AAC/Opus
  ├─ packet ring buffer (RAM, byte-capped; optional disk segments)
  ├─ hotkeys (RegisterHotKey + Raw Input sink + XInput combo)
  ├─ overlay + toasts (DirectComposition, non-activating, excluded from capture)
  └─ saver thread (EcoQoS, low-I/O priority) → libavformat remux → destinations

clipui.exe  (launched on demand: settings + clip library + trimmer; exits when closed)
  └─ talks to clipd over a named pipe
```
- **Idle target:** a few MB private memory for `clipd` (plus the ring buffer), ~0% CPU when no game is running.
- **UI framework:** the UI process is short-lived, so choose for developer speed (WinUI 3, Qt, or Tauri). Only `clipd` must be minimal.

### 6.2 Pipeline
```
 Game ──Present──► DWM ──► WGC FrameArrived / DDA AcquireNextFrame   (QPC timestamps)
                     ▼
   [D3D11 device on the adapter that owns the game's monitor (LUID match)]
   CopyResource → small texture ring → fixed 30/60/120 Hz tick takes latest frame
                     ▼
   GPU colour convert: BGRA8→NV12  |  FP16 scRGB→P010 PQ (HDR) or tonemap→NV12
                     ▼
   HW encoder: NVENC | AMF | QSV | Media Foundation HW MFT   (IDR every 1 s, no B-frames, capped VBR)
                     ▼ compressed packets (small)
   Audio: WASAPI process loopback (game tree) / endpoint loopback / mic → AAC|Opus, own tracks
                     ▼
   Packet ring (preallocated, evicts whole GOPs)  [opt-in: disk segment ring]
                     ▼ hotkey: snapshot refs from last IDR ≤ (now − N)
   Saver: remux → fragmented MP4 / MKV (+ post-roll appended live) → destination(s) → toast/sound
```

### 6.3 Key decisions & why
| Area | Choice | Reason |
|---|---|---|
| Capture | **WGC** primary, **DDA** fallback (Win10 monitor capture, no yellow border) | OS-sanctioned, no injection → anti-cheat safe. Most "exclusive fullscreen" DX11/12 games actually run flip-model under Fullscreen Optimizations, which WGC captures. |
| Injection / game hooks | **None in v1** | Vanguard, EAC, FACEIT, and BattlEye conflicts are the main source of black screens and bans fear. At most a later opt-in for non-anti-cheat true-exclusive games. |
| Frame rate | Fixed encode tick (30/60/120, user-chosen), `MinUpdateInterval` on Win11 24H2+ | Decouples from 300+ fps and VRR jitter; caps GPU work. |
| Encode | libavcodec with D3D11 hardware frames (`*_nvenc`, `*_amf`, `*_qsv`) → native SDKs later if needed; MF HW MFT fallback; x264 opt-in only | One code path for all three vendors, zero-copy. Software encode is what kills FPS. |
| Default codec | HEVC (AV1 where supported; H.264 "max compatibility") + automatic H.264 share-export | Best quality per MB. Export handles Discord and browser playback. |
| Buffer | Compressed packets in RAM, byte-capped | 50 Mbps × 2 min ≈ 750 MB; 20 Mbps × 1 min ≈ 150 MB. Audio ≈ 3 MB per track per 2 min. |
| Save | Remux only, fragmented MP4/MKV, low-priority thread | No re-encode → instant, no GPU/CPU spike, crash-safe. |
| Audio | Process loopback (Win11 build 20348+), endpoint loopback fallback, mic separate; QPC timestamps; silence-fill; device hot-plug rebuild | Per-app tracks, survives headset swaps, A/V sync. |
| Hotkeys | `RegisterHotKey` for save; Raw Input sink for chords; XInput combo when a game is detected; **no `WH_KEYBOARD_LL`** | Works whatever has focus, zero input latency, no keylogger flags. |
| Overlay | Topmost non-activating DirectComposition window, `WDA_EXCLUDEFROMCAPTURE`; never injected | Works over borderless and fullscreen-optimized games. In true exclusive FS, fall back to sound cues. Never steals focus. |
| Priorities | Encode-submit thread above normal; audio MMCSS; saver/thumbnails EcoQoS + background I/O; GPU priority normal, raised adaptively to HIGH only if frames drop; **never REALTIME** | Game first. |
| Idle | Foreground-window event hook + fullscreen heuristics + known launcher paths + allow/deny list → tear down capture/encoder when no game | Near-zero cost on the desktop. "Always record desktop" is optional. |
| Language | **Rust** (windows-rs, windows-capture crate, ffmpeg bindings) or **C++20** | Rust: memory safety for a long-running service. C++: native fit with the vendor SDKs and OBS reference code. |
| OS target | **Windows 11 primary**, Windows 10 22H2 best-effort | Win10 left support Oct 2025. Win11 gives a borderless WGC window, process loopback, and `MinUpdateInterval` (24H2). |

### 6.4 Performance budget (proposed; publish measured results)
- **No game running:** < 0.2% CPU, < 15 MB private memory (excluding the buffer), 0 GPU work.
- **Recording 1080p60 / 1440p60 HEVC:**
  - ≤ 1–2% average FPS loss, ≤ 2% 1%-low loss.
  - < 1% CPU (total system).
  - RAM = buffer size + < 30 MB.
- **On save:** no measurable frame-time spike. Verify with PresentMon and CapFrameX on NVIDIA, AMD, and Intel, and on a hybrid laptop.

### 6.5 Top technical risks → mitigations
1. **True exclusive-fullscreen game is not capturable.** Detect black or stalled frames and tell the user "switch to borderless" with a one-line explanation. Never fail silently.
2. **Yellow capture border (Win10) / borderless consent prompt (Win11).** Use DDA monitor capture on Win10. Handle denied consent gracefully.
3. **GPU at 100% starves the capture copy.** Keep GPU work to a single copy + convert, cap the capture fps, raise GPU priority adaptively, expose a "dropped frames" counter.
4. **Hybrid laptop adapter mismatch.** Match LUIDs and re-check on display changes.
5. **Encoder availability / NVENC session limits.** Probe capabilities at startup; fall back through the chain.
6. **HDR colour.** Capture FP16, then either correct PQ + HDR10 metadata or tone-map to SDR. Test with Auto HDR.
7. **A/V desync and device hot-plug.** QPC everywhere, silence fill, device-change rebuild, resample when mixing.
8. **Device lost, driver update, resolution change.** Rebuild without clearing the ring; start a new segment with new parameter sets.
9. **Anti-cheat or antivirus false positives.** No injection, no low-level hooks, no game process handles, code-signed; test matrix of Vanguard, EAC, BattlEye, FACEIT, and Ricochet titles.
10. **Memory or stutter during save.** Preallocated arena, O(1) snapshot, remux on an EcoQoS thread, disk ring opt-in (warn about SSD wear: ~22 GB/hour at 50 Mbps).

---

## 7. Suggested in-game quick panel (overlay)

The panel opens on a chord, appears as a small slide-in from a screen edge, and never takes focus. Keyboard navigation uses temporary hotkeys (arrows, Enter, Esc) that exist only while the panel is open. Clicking also works.

- **Status:** ● Recording · 60 s buffer · 1440p60 HEVC · 412 MB RAM
- **Clip length:** 15 s / 30 s / 60 s / 2 min / custom
- **Quality preset:** Performance / Balanced / Quality
- **Audio toggles:** Game · Voice chat · Mic · Other apps
- **Pause buffer** (privacy) · **Save now** · **Open last clip**
- **Destination:** quick switch between saved locations
- **Per-game profile:** "Save these settings for _this game_"

Default hotkeys must not collide with the NVIDIA (Alt+Z, Alt+F10), AMD (Alt+R), Game Bar (Win+Alt+G), or Steam (F12, Shift+F11) defaults. Detect conflicts at first run and let the user choose.

---

## 8. Suggested phases
1. **MVP:**
   - WGC/DDA capture, HW encode (via libavcodec), RAM packet ring, save hotkey with remux.
   - System audio + mic on separate tracks.
   - Tray icon with health status, toast + sound, save folder, clip length & quality settings.
   - Benchmark harness (PresentMon).
2. **v1:**
   - Per-app audio tracks, per-game profiles, quick-panel overlay, controller combo.
   - Storage quota & auto-prune, folder-per-game naming, clip library + lossless trim.
   - Discord-ready export, HDR handling, multi-destination saving.
3. **v1.x:**
   - Markers + timeline / long buffer with disk ring.
   - Screenshot hotkey, CLI/IPC + Stream Deck plugin, post-save scripts.
   - Battery-aware mode.
4. **Later:**
   - Local auto-clipping via official game APIs (CS2/Dota GSI, LoL Live Client) and a voice trigger.
   - Vertical export.
   - Optional opt-in hook capture for non-anti-cheat exclusive-fullscreen titles.

---

## 9. Decisions
Resolved in [decisions.md](decisions.md). Where a decision differs from this report (notably D4: no FFmpeg, Media Foundation + native encoders and our own muxer), decisions.md wins.

---

## 10. Reference code to study
- **OBS:**
  - [libobs-winrt/winrt-capture.cpp](https://github.com/obsproject/obs-studio/blob/master/libobs-winrt/winrt-capture.cpp): WGC, HDR, border.
  - [plugins/win-capture](https://github.com/obsproject/obs-studio/tree/master/plugins/win-capture): DDA and game hook (for reference only).
  - The NVENC/AMF/QSV encoder plugins and the replay buffer in `obs-ffmpeg`.
- **Sunshine:** [src/platform/windows](https://github.com/LizardByte/Sunshine/tree/master/src/platform/windows): DDA/WGC, HDR, GPU priority, hybrid-GPU handling.
- **[windows-capture](https://github.com/NiiightmareXD/windows-capture)** (Rust, WGC + DDA).
- **[robmikh/Win32CaptureSample](https://github.com/robmikh/Win32CaptureSample)** and Microsoft's **[ApplicationLoopback sample](https://github.com/microsoft/windows-classic-samples/tree/main/Samples/ApplicationLoopback)**.
- **[rbuf](https://github.com/r3clusionn/rbuf)** (Rust packet ring, per-process audio tracks), **[OpenReplay](https://github.com/G4F-Elite/OpenReplay)**, **[trix](https://github.com/tnhnblgl/trix)**.
- **[gpu-screen-recorder](https://git.dec05eba.com/gpu-screen-recorder/about/)** (Linux, but good replay/IPC design ideas).

### Key sources
- NVIDIA App release highlights: https://www.nvidia.com/en-us/software/nvidia-app/release-highlights/
- AMD replay FAQ: https://www.amd.com/en/resources/support-articles/faqs/DH3-023.html
- Steam HDR thread: https://steamcommunity.com/groups/SteamClientBeta/discussions/3/6918175214322789660
- Steam storage thread: https://steamcommunity.com/groups/SteamClientBeta/discussions/5/6324829191798551900
- Tom's Hardware, NVIDIA App perf: https://tomshardware.com/pc-components/gpus/we-tested-nvidia-app-performance-problems-games-run-up-to-15-percent-slower-with-the-app
- Medal reviews: https://uk.trustpilot.com/review/medal.tv
- OBS replay-save-fails issue #12080: https://obs-versions.com/community/issues/12080
- WGC `IsBorderRequired`: https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession.isborderrequired
- WGC `MinUpdateInterval`: https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession.minupdateinterval
- Process loopback: https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ns-audioclientactivationparams-audioclient_process_loopback_params
- Fullscreen Optimizations: https://devblogs.microsoft.com/directx/demystifying-full-screen-optimizations/
- NVENC app note: https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-application-note/index.html
