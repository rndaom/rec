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
| `--recd <path>` | `recd.exe` to start for the "on" phases (default: next to `rec-bench.exe`). |
| `--save-every <secs>` | Also save a 10 s clip every N seconds during "on" captures and report the frame-time spike around each save. The clips are deleted afterwards. |
| `--label <text>` | Names the results folder (default: the process name). |
| `--out <dir>` | Where results go (default: `bench/results`). |

What happens, per run:
1. **Off:** `recd` is stopped (asked to quit over IPC). PresentMon captures the game.
2. **On:** `recd` is started and `rec-bench` waits up to 30 s for `rec status` to say `recording`, so the game must be running and in the foreground. PresentMon captures the game while `rec-bench` samples `recd`'s CPU and memory.

The order alternates between runs (off, on / on, off / off, on ...) so slow drift such as heat soak does not favour either side.

If `recd` was already running, `rec-bench` stops it for the benchmark and starts it again at the end, from the same file. Ctrl+C also restores it. `rec-bench` only ever stops `recd` by asking it to quit, and only kills the PresentMon processes it started itself.

### Making the numbers trustworthy
- Same scene every time: a replay, a benchmark mode, or standing still in a fixed spot. Load and menu screens give meaningless numbers.
- Fixed resolution, refresh rate and graphics settings. Do not change them between runs.
- V-Sync off and the frame rate uncapped, or capped at the same value for every run. A frame cap hides loss: the game just has more headroom.
- Prefer a GPU-bound scene, because that is where capture and encode contend with the game. Also test a CPU-bound one.
- Close browsers, launchers and overlays. Pause updates and downloads.
- Let the game warm up (shader compilation, asset streaming) before starting. Raise `--warmup` if the first seconds look different from the rest.
- Use at least 3 runs, ideally 5. With fewer, `rec-bench` warns that run-to-run noise is unknown. If the standard deviation of the deltas is about as large as the deltas, you are measuring noise.
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

prints the statistics of PresentMon CSVs. [CapFrameX](https://www.capframex.com/) captures can be fed in too if you export them as CSV and the file has PresentMon-style columns. The frame-time column is found by name (`MsBetweenPresents`, `FrameTime`, or v1's `msBetweenPresents`). Rows with `NA` are skipped. If the file holds several applications, the one with the most frames is used; pass `--process` to choose. If the app used several swap chains, the busiest one is used.

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

The report starts with the system (CPU, OS build, the GPU/encoder string from `rec status`) and the rec settings used, then a pass/fail verdict:

- **Average FPS loss** and **1% low loss** are the means over the runs; a gain counts as a pass.
- **recd CPU** is the mean over the "on" phases.
- **0.1% low** is reported but has no budget.
- The save-spike table is informational.

If a run fails or is interrupted after at least one full run, the report is still written and marked **Incomplete**.

A folder that already exists is never overwritten: a numeric suffix is added.

## Troubleshooting
- *PresentMon was not found:* see Setup.
- *PresentMon exited with an access error:* see the permissions step in Setup.
- *Only N frames were captured:* the process name is wrong, or the game was minimised or not presenting. PresentMon only sees processes that present frames.
- *recd did not become recording:* the game must be running and in the foreground before the "on" phase, and `recd` must detect it. Check with `rec status` and `%LOCALAPPDATA%\rec\recd.log`.
- *Save requests fail:* the notes section of the report lists the error `recd` returned.
