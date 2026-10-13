# Benchmarking rec

`rec-bench` measures what recording costs a game, against the budget in [decisions.md](../docs/decisions.md) (D11):

| | Budget |
|---|---|
| No game running | < 0.2% CPU, < 15 MB private memory |
| Recording | ≤ 2% average-FPS loss, ≤ 2% 1%-low loss, `recd` < 1% of total CPU |
| On save | No frame-time spike |

It runs [PresentMon](https://github.com/GameTechDev/PresentMon) on the game with `recd` off and on, alternating, then compares frame rate and frame pacing. It also reads `recd`'s own CPU time and memory.

## Setup
1. Build: `cargo build --release`. This makes `target\release\rec-bench.exe` next to `recd.exe`.
2. Download the PresentMon **console application** (2.x) from <https://github.com/GameTechDev/PresentMon/releases>. Put the `.exe` next to `rec-bench.exe` or on `PATH`, or pass `--presentmon <path>`.
3. Make sure PresentMon is allowed to read ETW events. It needs one of:
   - an elevated (administrator) terminal, or
   - your account in the **Performance Log Users** group (`lusrmgr.msc`, or `net localgroup "Performance Log Users" %USERNAME% /add` from an admin terminal), then sign out and in.

   Without this PresentMon fails with an access error and `rec-bench` repeats this hint.

## Running a benchmark
Start the game first and get into the scene you want to test. Then, from a terminal:

```
rec-bench run --process Deadlock.exe --label "deadlock-1440p"
```

`--process` is the executable name exactly as Task Manager shows it. Defaults: 60 s per capture, 3 runs, 5 s warm-up. Useful options:

| Option | |
|---|---|
| `--duration <secs>` | Length of each capture (default 60). |
| `--runs <n>` | Number of off/on pairs (default 3). |
| `--warmup <secs>` | Settle time before every capture (default 5). |
| `--on-warmup <secs>` | Minimum time a freshly started `recd` records before an "on" capture (default: the replay length in your rec config, 60 s if the config cannot be read; never below `--warmup`). |
| `--recd <path>` | `recd.exe` to start for the "on" phases (default: next to `rec-bench.exe`). |
| `--save-every <secs>` | Also save a 10 s clip every N seconds during "on" captures and report the frame-time spike around each save. The clips are deleted afterwards. |
| `--label <text>` | Names the results folder (default: the process name). |
| `--out <dir>` | Where results go (default: `bench/results`). |

What happens, per run:
1. **Off:** `recd` is stopped (asked to quit over IPC). PresentMon captures the game.
2. **On:** `recd` is restarted (before *every* "on" phase, even if the previous phase left one running) and `rec-bench` waits up to 30 s for `rec status` to say `recording`, so the game must be running and in the foreground. It then warms up for at least the replay length (`--on-warmup`), so the replay buffer is full when the capture starts and every "on" capture sees the same steady state. PresentMon captures the game while `rec-bench` samples `recd`'s CPU and memory. The report records, per run, that `recd` was fresh and for how long it was warmed up, and how much the buffer held.

The order alternates between runs (off, on / on, off / off, on ...) so slow drift such as heat soak does not favour either side.

Running time: each run takes about `duration + warm-up` for the off phase and `duration + max(warm-up, replay length)` for the on phase, plus a few seconds to restart `recd`. With the defaults (60 s captures, 60 s replay buffer, 3 runs) that is roughly 3 x (65 + 120) s, about 9 minutes. Lower `--on-warmup` to save time, at the cost of capturing while the buffer is still filling.

If `recd` was already running, `rec-bench` stops it for the benchmark and starts it again at the end, from the same file. Ctrl+C also restores it. `rec-bench` only ever stops `recd` by asking it to quit and then waiting for that process to exit (it never polls `recd`'s pipe afterwards, which can hang while `recd` shuts down). It only kills the PresentMon processes it started itself, and then also ends the `rec-bench` ETW session PresentMon leaves behind (`--terminate_existing_session`). On Ctrl+C PresentMon first gets a few seconds to exit on its own.

`recd`'s process is identified by the owner of its named pipe, not by its file name. If the pipe cannot be inspected and several processes are named `recd.exe`, `rec-bench` stops with an error instead of sampling one of them at random.

### Running as administrator
PresentMon needs an elevated terminal or the **Performance Log Users** group (see Setup). If you run `rec-bench` elevated, the `recd` it starts is elevated too, which has two consequences:

- The report says so (System table and Notes): the "on" phases ran `recd` as administrator.
- `rec-bench` does **not** start your own `recd` again at the end. A `recd` started from an elevated process gets an admin-only pipe, and `rec save` from a normal terminal would stop working. It prints the path and a note in the report instead; start `recd` yourself from a normal terminal.

The Performance Log Users group avoids all of this: run `rec-bench` from a normal terminal and your `recd` is restored as it was.

### Making the numbers trustworthy
- Same scene every time: a replay, a benchmark mode, or standing still in a fixed spot. Load and menu screens give meaningless numbers.
- Fixed resolution, refresh rate and graphics settings. Do not change them between runs.
- V-Sync off and the frame rate uncapped, or capped at the same value for every run. A frame cap hides loss: the game just has more headroom.
- Prefer a GPU-bound scene, because that is where capture and encode contend with the game. Also test a CPU-bound one.
- Close browsers, launchers and overlays. Pause updates and downloads.
- Let the game warm up (shader compilation, asset streaming) before starting. Raise `--warmup` if the first seconds look different from the rest.
- Use at least 3 runs, ideally 5. With fewer, the run-to-run noise is unknown and the verdict is INCONCLUSIVE unless the result is far from the budget (see below). If the standard deviation of the deltas is about as large as the deltas, you are measuring noise.
- Keep `recd`'s config the same as you would ship it. The summary records the replay length, fps, codec, quality and bitrate it used.
- Do not touch the machine while it runs.

## Idle cost
With no game running:

```
rec-bench idle
```

It starts `recd` if it is not running (and stops it again), checks that its state is `idle`, waits for the warm-up, then measures for 60 s (`--duration`) and compares with the D11 idle budget.

## Looking at existing captures
```
rec-bench analyze capture1.csv capture2.csv [--process Game.exe]
```

prints the statistics of PresentMon CSVs. [CapFrameX](https://www.capframex.com/) captures can be fed in too if you export them as CSV and the file has PresentMon-style columns. The frame-time column is found by name (`MsBetweenPresents`, `FrameTime`, or v1's `msBetweenPresents`). Rows with `NA` are skipped. PresentMon 2.x writes extra rows for generated frames (frame generation) and marks them in a `FrameType` column; when that column exists only the `Application` rows are used and the report says how many were ignored. If the file holds several applications, the one with the most frames is used; pass `--process` to choose. If the app used several swap chains, the busiest one is used.

## What the numbers mean
- **Average FPS:** frames divided by the sum of their frame times.
- **1% low / 0.1% low FPS:** `1000 / mean` of the slowest 1% / 0.1% of frame times in ms. This is CapFrameX's "average of the slowest N%", not a percentile. It describes how bad the worst moments are.
- **Median / P99 frame time:** the typical frame and the 99th-percentile frame (nearest rank, so an actual observed frame).
- **Stutters:** frames longer than 2.5 times the median.
- **Change:** `(on − off) / off`. Negative means recording made the game slower. The mean is the mean of the per-run changes.
- **recd CPU:** CPU time used by `recd` while recording, as a share of the whole machine (all logical cores) and of one core. Taken from `GetProcessTimes` over the capture.
- **Private memory:** `recd`'s private bytes (what Task Manager calls commit size), sampled once a second. MB here means MiB.
- **Save spike:** the slowest frame from 1 s before a save request to 1 s after it finished, compared with that capture's median. A ratio above 2.5 means a stutter-sized frame coincided with the save. Compare with the "off" captures' own worst frames before blaming the save. Needs `CPUStartQPC` timestamps, which `rec-bench` asks PresentMon for with `--qpc_time`.

## Reading the output
`rec-bench run` writes `bench/results/<YYYY-MM-DD>-<label>/`:

| File | |
|---|---|
| `summary.md` | The human-readable report: system, config, verdict, per-run tables. It is also printed at the end. |
| `summary.json` | The same data for scripts. |
| `raw/` | PresentMon CSVs and logs for every phase. Git-ignored; they are large. |

The report starts with the system (CPU, OS build, the GPU/encoder string from `rec status`, whether it ran as administrator) and the rec settings used, then a verdict, which is **PASS**, **FAIL** or **INCONCLUSIVE**:

- **Average FPS loss** and **1% low loss** are the means over the runs; a gain counts as a pass.
- **recd CPU** is the mean over the "on" phases.
- **0.1% low** is reported but has no budget.
- The save-spike table is informational. It also lists, per run, how many saves were requested and how many failed.

How a check is decided: next to each measured mean the report shows `�`, the noise on that mean: `2 x s / sqrt(n)`, where `s` is the standard deviation across the `n` runs (roughly a 95% interval).

| Runs | Rule |
|---|---|
| 3 or more | PASS or FAIL if the mean is farther from the budget than the noise. If it is within the noise, INCONCLUSIVE. Identical runs sitting exactly on the limit are decided by the limit itself (<= for the FPS losses, < for CPU). |
| 1 or 2 | The noise cannot be estimated, so INCONCLUSIVE, unless the mean is at least 5 points from the limit for an FPS loss (a loss of 7% or more fails, a gain of 3% or more passes) or 0.5 points for `recd`'s CPU share (1.5% or more fails, 0.5% or less passes). |

The overall verdict is FAIL if any check fails, otherwise INCONCLUSIVE if any is inconclusive, otherwise PASS. INCONCLUSIVE means "run more runs (5 or more) or on a quieter machine"; it is not a pass.

If a run fails or is interrupted after at least one full run, the report is still written and marked **Incomplete**.

A folder that already exists is never overwritten: a numeric suffix is added.

## Troubleshooting
- *PresentMon was not found:* see Setup.
- *PresentMon exited with an access error:* see the permissions step in Setup.
- *Only N frames were captured:* the process name is wrong, or the game was minimised or not presenting. PresentMon only sees processes that present frames.
- *recd did not become recording:* the game must be running and in the foreground before the "on" phase, and `recd` must detect it. Check with `rec status` and `%LOCALAPPDATA%\rec\recd.log`.
- *Save requests fail:* the notes section of the report lists the error `recd` returned, and the save table shows requested against failed.
- *A stale ETW session after a crash:* `rec-bench` ends its own `rec-bench` session when it kills PresentMon. If a hard crash left one behind, run `PresentMon --terminate_existing_session --session_name rec-bench`; the next capture also replaces it.
