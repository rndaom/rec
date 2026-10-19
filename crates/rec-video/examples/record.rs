//! Records the raw Annex-B packet stream of the rec-video pipeline to a file
//! and prints pipeline statistics plus process CPU time and memory.
//!
//! ```text
//! cargo run --release -p rec-video --example record -- --seconds 10 --fps 60 \
//!     [--monitor N | --window-title "<substring>"] [--capture auto|wgc|dda] \
//!     [--codec h264|hevc|av1|auto] \
//!     [--encoder auto|amf|mf] [--hdr auto|tonemap|off] [--assume-hdr] \
//!     [--bitrate KBPS] [--max-height N] [--no-cursor] [--request-borderless] \
//!     --out out.h265
//! ```
//!
//! `--capture dda` forces Desktop Duplication (the Windows 10 path: the
//! display cropped to the window) on Windows 11. `--assume-hdr` runs the HDR path (FP16 capture, HDR10 or tone-mapped
//! output) on an SDR display. AV1 is written as a low-overhead OBU stream
//! (`ffprobe -f obu out.obu`).

#[cfg(not(windows))]
fn main() {
    eprintln!("this example only runs on Windows");
}

#[cfg(windows)]
fn main() {
    if let Err(e) = imp::main() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
mod imp {
    use std::fs::File;
    use std::io::{BufWriter, Write};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use rec_video::{
        CaptureApi, CaptureSource, CodecPreference, EncoderBackend, HdrMode, Packet, VideoCapture,
        VideoCaptureConfig, annexb, obu,
    };
    use windows::Win32::Foundation::{
        CloseHandle, FILETIME, HLOCAL, HWND, LPARAM, LocalFree, RECT,
    };
    use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};
    use windows::Win32::Media::MediaFoundation::{
        MF_VERSION, MFSTARTUP_FULL, MFShutdown, MFStartup,
    };
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetProcessTimes, GetThreadDescription, GetThreadTimes, OpenThread,
        THREAD_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW, IsWindowVisible};
    use windows::core::BOOL;

    #[derive(Default)]
    struct Args {
        seconds: u64,
        fps: u32,
        monitor: Option<usize>,
        window_title: Option<String>,
        capture: CaptureApi,
        codec: CodecPreference,
        encoder: EncoderBackend,
        hdr: HdrMode,
        assume_hdr: bool,
        bitrate: u32,
        max_height: Option<u32>,
        cursor: bool,
        request_borderless: bool,
        out: String,
    }

    fn parse_args() -> Result<Args, String> {
        let mut a = Args {
            seconds: 10,
            fps: 60,
            bitrate: 20_000,
            cursor: true,
            out: "out.h265".into(),
            ..Default::default()
        };
        let mut it = std::env::args().skip(1);
        while let Some(flag) = it.next() {
            let mut value = || it.next().ok_or(format!("{flag} needs a value"));
            let num = |s: String| s.parse::<u64>().map_err(|e| format!("{s}: {e}"));
            match flag.as_str() {
                "--seconds" => a.seconds = num(value()?)?,
                "--fps" => a.fps = num(value()?)? as u32,
                "--monitor" => a.monitor = Some(num(value()?)? as usize),
                "--window-title" => a.window_title = Some(value()?),
                "--capture" => {
                    a.capture = match value()?.as_str() {
                        "auto" => CaptureApi::Auto,
                        "wgc" => CaptureApi::Wgc,
                        "dda" => CaptureApi::Dda,
                        other => return Err(format!("unknown capture API {other}")),
                    }
                }
                "--codec" => {
                    a.codec = match value()?.as_str() {
                        "h264" => CodecPreference::H264,
                        "hevc" | "h265" => CodecPreference::Hevc,
                        "av1" => CodecPreference::Av1,
                        "auto" => CodecPreference::Auto,
                        other => return Err(format!("unknown codec {other}")),
                    }
                }
                "--encoder" => {
                    a.encoder = match value()?.as_str() {
                        "auto" => EncoderBackend::Auto,
                        "amf" => EncoderBackend::Amf,
                        "mf" => EncoderBackend::MediaFoundation,
                        other => return Err(format!("unknown encoder {other}")),
                    }
                }
                "--hdr" => {
                    a.hdr = match value()?.as_str() {
                        "auto" => HdrMode::Auto,
                        "tonemap" => HdrMode::Tonemap,
                        "off" => HdrMode::Off,
                        other => return Err(format!("unknown HDR mode {other}")),
                    }
                }
                "--assume-hdr" => a.assume_hdr = true,
                "--bitrate" => a.bitrate = num(value()?)? as u32,
                "--max-height" => a.max_height = Some(num(value()?)? as u32),
                "--no-cursor" => a.cursor = false,
                "--request-borderless" => a.request_borderless = true,
                "--out" => a.out = value()?,
                other => return Err(format!("unknown argument {other}")),
            }
        }
        Ok(a)
    }

    fn monitors() -> Vec<isize> {
        unsafe extern "system" fn cb(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
            // SAFETY: `data` is the `&mut Vec` passed below, alive for the call.
            let list = unsafe { &mut *(data.0 as *mut Vec<isize>) };
            list.push(m.0 as isize);
            true.into()
        }
        let mut list: Vec<isize> = Vec::new();
        // SAFETY: the callback only touches `list`, which outlives the call.
        let _ = unsafe {
            EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut list as *mut _ as isize))
        };
        list
    }

    fn find_window(needle: &str) -> Option<(isize, String)> {
        struct Search {
            needle: String,
            found: Option<(isize, String)>,
        }
        unsafe extern "system" fn cb(hwnd: HWND, data: LPARAM) -> BOOL {
            // SAFETY: `data` is the `&mut Search` passed below.
            let search = unsafe { &mut *(data.0 as *mut Search) };
            // SAFETY: plain window queries on an enumerated HWND.
            if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
                return true.into();
            }
            let mut buf = [0u16; 512];
            // SAFETY: as above; the buffer is valid for writes.
            let n = unsafe { GetWindowTextW(hwnd, &mut buf) }.max(0) as usize;
            let title = String::from_utf16_lossy(&buf[..n]);
            if title.to_lowercase().contains(&search.needle) {
                search.found = Some((hwnd.0 as isize, title));
                return false.into();
            }
            true.into()
        }
        let mut search = Search {
            needle: needle.to_lowercase(),
            found: None,
        };
        // SAFETY: the callback only touches `search`, which outlives the call.
        let _ = unsafe { EnumWindows(Some(cb), LPARAM(&mut search as *mut _ as isize)) };
        search.found
    }

    fn filetime(t: FILETIME) -> f64 {
        ((u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)) as f64 / 1e7
    }

    fn cpu_seconds() -> f64 {
        let (mut c, mut e, mut k, mut u) = Default::default();
        // SAFETY: pseudo-handle of this process; out-pointers valid.
        let _ = unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
        filetime(k) + filetime(u)
    }

    /// CPU seconds and description of every thread of this process.
    fn thread_cpu() -> Vec<(u32, String, f64)> {
        let mut out = Vec::new();
        // SAFETY: snapshot of this process's threads; handle closed below.
        let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }) else {
            return out;
        };
        let pid = std::process::id();
        let mut e = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        // SAFETY: `e` has its size set; the snapshot handle is valid.
        let mut ok = unsafe { Thread32First(snap, &mut e) }.is_ok();
        while ok {
            if e.th32OwnerProcessID == pid {
                // SAFETY: query-only handle to one of our own threads.
                let handle =
                    unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, false, e.th32ThreadID) };
                if let Ok(h) = handle {
                    let (mut c, mut x, mut k, mut u) = Default::default();
                    // SAFETY: valid thread handle; out-pointers valid.
                    let _ = unsafe { GetThreadTimes(h, &mut c, &mut x, &mut k, &mut u) };
                    let mut name = String::new();
                    // SAFETY: valid thread handle. The description is a
                    // LocalAlloc'd NUL-terminated string, read then freed.
                    if let Ok(p) = unsafe { GetThreadDescription(h) } {
                        // SAFETY: valid NUL-terminated string from the OS.
                        name = unsafe { p.to_string() }.unwrap_or_default();
                        // SAFETY: freed once, as GetThreadDescription requires.
                        let _ = unsafe { LocalFree(Some(HLOCAL(p.0.cast()))) };
                    }
                    out.push((e.th32ThreadID, name, filetime(k) + filetime(u)));
                    // SAFETY: we own the handle.
                    let _ = unsafe { CloseHandle(h) };
                }
            }
            // SAFETY: as above.
            ok = unsafe { Thread32Next(snap, &mut e) }.is_ok();
        }
        // SAFETY: we own the snapshot handle.
        let _ = unsafe { CloseHandle(snap) };
        out
    }

    fn memory() -> PROCESS_MEMORY_COUNTERS_EX {
        let mut m = PROCESS_MEMORY_COUNTERS_EX {
            cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        };
        // SAFETY: the struct is the size we pass; EX extends the base layout.
        let _ = unsafe {
            GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut m as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
                m.cb,
            )
        };
        m
    }

    #[derive(Default)]
    struct Tally {
        packets: u64,
        bytes: u64,
        keyframes: u64,
        keyframes_with_params: u64,
        keyframe_pts: Vec<i64>,
        first_pts: Option<i64>,
        last_pts: Option<i64>,
        non_monotonic: u64,
        io_error: Option<String>,
    }

    pub fn main() -> Result<(), String> {
        let args = parse_args()?;
        // SAFETY: process-wide setting made before any window/monitor query.
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        // SAFETY: the caller of rec-video starts Media Foundation once.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.map_err(|e| format!("MFStartup: {e}"))?;

        if args.request_borderless {
            println!(
                "RequestAccessAsync(Borderless) -> {:?}",
                rec_video::request_borderless_access()
            );
        }

        let source = if let Some(title) = &args.window_title {
            let (hwnd, full) =
                find_window(title).ok_or(format!("no visible window title contains {title:?}"))?;
            println!("source: window {hwnd:#x} \"{full}\"");
            CaptureSource::Window(hwnd)
        } else if let Some(i) = args.monitor {
            let list = monitors();
            let m = *list
                .get(i)
                .ok_or(format!("monitor {i} not found ({} total)", list.len()))?;
            println!("source: monitor #{i} ({m:#x}) of {}", list.len());
            CaptureSource::Monitor(m)
        } else {
            println!("source: primary monitor");
            CaptureSource::PrimaryMonitor
        };

        let config = VideoCaptureConfig {
            capture: args.capture,
            fps: args.fps,
            max_height: args.max_height,
            codec: args.codec,
            encoder: args.encoder,
            hdr: args.hdr,
            assume_hdr_display: args.assume_hdr,
            bitrate_kbps: args.bitrate,
            cursor: args.cursor,
            ..VideoCaptureConfig::new(source)
        };

        let file = File::create(&args.out).map_err(|e| format!("{}: {e}", args.out))?;
        let tally = Arc::new(Mutex::new(Tally::default()));
        let mut writer = BufWriter::with_capacity(1 << 20, file);
        let sink_tally = tally.clone();
        // The codec is only known after start; detect it from the stream.
        let sink = Box::new(move |p: Packet| {
            let mut t = sink_tally.lock().unwrap();
            if let Err(e) = writer.write_all(&p.data) {
                t.io_error = Some(e.to_string());
            }
            if p.keyframe {
                // Whole GOPs reach the disk even if the process dies.
                let _ = writer.flush();
            }
            t.packets += 1;
            t.bytes += p.data.len() as u64;
            if p.keyframe {
                t.keyframes += 1;
                t.keyframe_pts.push(p.pts);
                // OBU type 1 is the AV1 sequence header.
                let with = annexb::has_parameter_sets(rec_video::VideoCodec::Hevc, &p.data)
                    || annexb::has_parameter_sets(rec_video::VideoCodec::H264, &p.data)
                    || obu::split_obus(&p.data).is_ok_and(|obus| obus.iter().any(|o| o.kind == 1));
                if with {
                    t.keyframes_with_params += 1;
                }
            }
            if t.last_pts.is_some_and(|l| p.pts <= l) || p.pts != p.dts {
                t.non_monotonic += 1;
            }
            t.first_pts.get_or_insert(p.pts);
            t.last_pts = Some(p.pts);
        });

        let cpu0 = cpu_seconds();
        let wall0 = Instant::now();
        let capture = VideoCapture::start(config, sink).map_err(|e| e.to_string())?;
        let info = capture.info().clone();
        println!(
            "adapter: {} (owns monitor: {})",
            info.adapter_name, info.adapter_owns_monitor
        );
        println!("capture: {:?}", info.capture);
        println!("encoder: {}", info.encoder_name);
        println!(
            "format: {} {}x{} @ {} fps (source {}x{})",
            info.format.codec.name(),
            info.format.width,
            info.format.height,
            info.format.fps,
            info.source_width,
            info.source_height
        );
        println!("colour: {:?}", info.format.color);
        println!("display in HDR mode: {}", info.display_hdr);
        for note in &info.notes {
            println!("note: {note}");
        }
        println!("rate control: {}", info.rate_control);
        println!(
            "border required: {:?}, min update interval: {:?} hns",
            info.border_required, info.min_update_interval_hns
        );

        let threads0 = thread_cpu();
        let steady_cpu0 = cpu_seconds();
        let steady_wall0 = Instant::now();
        let mut last_bytes = 0;
        for s in 1..=args.seconds {
            std::thread::sleep(Duration::from_secs(1));
            let st = capture.stats();
            let bytes = tally.lock().unwrap().bytes;
            println!(
                "[{s:>3}s] captured {} submitted {} (repeated {}) encoded {} dropped {} | {:.1} Mbit/s{}{}",
                st.frames_captured,
                st.frames_submitted,
                st.frames_repeated,
                st.frames_encoded,
                st.frames_dropped,
                (bytes - last_bytes) as f64 * 8.0 / 1e6,
                st.black_since
                    .map(|t| format!(
                        " | black for {:.1} s",
                        (rec_core::time::qpc_now() - t) as f64 / 1e7
                    ))
                    .unwrap_or_default(),
                st.last_error
                    .as_deref()
                    .map(|e| format!(" | error: {e}"))
                    .unwrap_or_default()
            );
            last_bytes = bytes;
            if !st.running {
                println!("pipeline stopped (source closed: {})", st.source_closed);
                break;
            }
        }
        let stats = capture.stats();
        let steady_cpu = cpu_seconds() - steady_cpu0;
        let steady_wall = steady_wall0.elapsed().as_secs_f64();
        let mut per_thread: Vec<(u32, String, f64)> = thread_cpu()
            .into_iter()
            .map(|(id, name, t)| {
                let before = threads0.iter().find(|x| x.0 == id).map_or(0.0, |x| x.2);
                (id, name, t - before)
            })
            .filter(|x| x.2 > 0.0)
            .collect();
        per_thread.sort_by(|a, b| b.2.total_cmp(&a.2));
        let stop0 = Instant::now();
        capture.stop();
        let stop_ms = stop0.elapsed().as_secs_f64() * 1e3;
        let wall = wall0.elapsed().as_secs_f64();
        let cpu = cpu_seconds() - cpu0;
        let mem = memory();
        // SAFETY: balanced with MFStartup above; all MF objects are released.
        let _ = unsafe { MFShutdown() };

        let t = tally.lock().unwrap();
        println!("--- final ---");
        println!("{stats:#?}");
        println!(
            "packets {} bytes {} ({:.2} Mbit/s avg) keyframes {} (with parameter sets {}) non-monotonic {}",
            t.packets,
            t.bytes,
            t.bytes as f64 * 8.0 / 1e6 / wall.max(0.001),
            t.keyframes,
            t.keyframes_with_params,
            t.non_monotonic
        );
        let gaps: Vec<f64> = t
            .keyframe_pts
            .windows(2)
            .map(|w| (w[1] - w[0]) as f64 / 1e7)
            .collect();
        println!("keyframe intervals (s): {gaps:.3?}");
        if let (Some(a), Some(b)) = (t.first_pts, t.last_pts) {
            println!("pts span {:.3} s", (b - a) as f64 / 1e7);
        }
        if let Some(e) = &t.io_error {
            println!("write error: {e}");
        }
        println!("stop() took {stop_ms:.1} ms");
        println!(
            "process CPU {:.3} s over {:.2} s wall = {:.2}% of one core (incl. setup/teardown)",
            cpu,
            wall,
            cpu / wall * 100.0
        );
        println!(
            "steady-state CPU {:.3} s over {:.2} s = {:.2}% of one core",
            steady_cpu,
            steady_wall,
            steady_cpu / steady_wall * 100.0
        );
        println!(
            "working set {:.1} MB (peak {:.1} MB), private {:.1} MB",
            mem.WorkingSetSize as f64 / 1048576.0,
            mem.PeakWorkingSetSize as f64 / 1048576.0,
            mem.PrivateUsage as f64 / 1048576.0
        );
        println!("steady-state CPU by thread (top 8):");
        for (id, name, t) in per_thread.iter().take(8) {
            println!(
                "  {id:>6} {:<24} {:.3} s ({:.2}% of one core)",
                if name.is_empty() { "-" } else { name },
                t,
                t / steady_wall * 100.0
            );
        }
        println!("wrote {}", args.out);
        Ok(())
    }
}
