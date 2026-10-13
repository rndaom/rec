//! Captures audio to ADTS `.aac` files (one per track) and prints stats and
//! CPU cost.
//!
//! ```text
//! cargo run --release -p rec-audio --example capture -- [--mix] <input>... <seconds> <out-prefix>
//! ```
//!
//! Inputs: `loopback` (all system audio), `mic` (default communications
//! capture device), `mic:ID` (a fixed endpoint id; a missing id records
//! silence, like an unplugged device), `pid:N` (what process N and its
//! children play), `app:NAME.exe` (follows that app, e.g. `app:Discord.exe`),
//! `not:N` (everything except process N). Each input gets its own track
//! (`<out-prefix>-<track>.aac`); `--mix` adds a mixed track 1 of all inputs.

#[cfg(not(windows))]
fn main() {
    eprintln!("rec-audio captures with WASAPI and only runs on Windows");
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("capture: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(windows)]
fn parse_source(spec: &str) -> Result<rec_audio::AudioSource, String> {
    use rec_audio::{AudioSource, ProcessTarget};

    let pid = |s: &str| s.parse::<u32>().map_err(|e| format!("{spec}: {e}"));
    Ok(match spec {
        "loopback" => AudioSource::SystemLoopback,
        "mic" => AudioSource::Microphone { device_id: None },
        _ => match spec.split_once(':') {
            Some(("mic", id)) => AudioSource::Microphone {
                device_id: Some(id.to_owned()),
            },
            Some(("pid", n)) => AudioSource::Process {
                target: ProcessTarget::Pid(pid(n)?),
                exclude: false,
            },
            Some(("not", n)) => AudioSource::Process {
                target: ProcessTarget::Pid(pid(n)?),
                exclude: true,
            },
            Some(("app", exe)) => AudioSource::Process {
                target: ProcessTarget::Executables(vec![exe.to_owned()]),
                exclude: false,
            },
            _ => return Err(format!("unknown input {spec:?}")),
        },
    })
}

#[cfg(windows)]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    use std::collections::BTreeMap;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use rec_audio::{AudioCapture, AudioCaptureConfig, AudioInput, AudioSource, aac};
    use rec_core::time::{as_secs_f64, qpc_now};
    use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_LITE, MFStartup};

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mix = args.first().is_some_and(|a| a == "--mix");
    if mix {
        args.remove(0);
    }
    let usage = "usage: capture [--mix] <input>... <seconds> <out-prefix>";
    let [specs @ .., seconds, out] = args.as_slice() else {
        return Err(usage.into());
    };
    if specs.is_empty() {
        return Err(usage.into());
    }
    let seconds: f64 = seconds.parse()?;
    let first_track = if mix { 2 } else { 1 };
    let inputs = specs
        .iter()
        .zip(first_track..)
        .map(|(spec, track)| {
            Ok(AudioInput {
                source: parse_source(spec)?,
                track: Some(track),
                mix,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;

    if inputs
        .iter()
        .any(|i| matches!(i.source, AudioSource::Microphone { .. }))
    {
        for d in rec_audio::list_microphones()? {
            println!(
                "mic: {}{}",
                d.name,
                if d.is_default { " (default)" } else { "" }
            );
        }
    }
    println!(
        "process loopback supported: {}",
        rec_audio::process_loopback_supported()
    );

    // SAFETY: once per process, before any Media Foundation use.
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }?;

    let (tx, rx) = mpsc::channel();
    let start_qpc = qpc_now();
    let capture = AudioCapture::start(
        AudioCaptureConfig {
            inputs,
            mix_track: mix.then_some(1),
            bitrate_kbps: 160,
        },
        Box::new(move |p| {
            let _ = tx.send(p);
        }),
    )?;
    let format = capture.format().clone();
    println!(
        "format: AAC-LC {} Hz, {} ch, {} kbit/s, AudioSpecificConfig {:02x?}, tracks {:?}",
        format.sample_rate,
        format.channels,
        capture.bitrate_kbps(),
        format.codec_config,
        capture.tracks(),
    );

    let cpu0 = cpu_time();
    let (cycles0, tsc0) = (cycles(), tsc());
    let wall0 = Instant::now();
    std::thread::sleep(Duration::from_secs_f64(seconds));
    let wall = wall0.elapsed();
    let cpu = cpu_time() - cpu0;
    let cycles = cycles() - cycles0;
    let tsc_hz = (tsc() - tsc0) as f64 / wall.as_secs_f64();
    let stats = capture.stats();
    capture.stop();
    let stop_qpc = qpc_now();

    let mut tracks: BTreeMap<u8, Vec<rec_core::replay::Packet>> = BTreeMap::new();
    for p in rx.try_iter() {
        tracks.entry(p.track).or_default().push(p);
    }
    println!("stats: {stats:#?}");
    for (track, packets) in &tracks {
        let mut file = Vec::new();
        let mut gaps = 0;
        for (i, p) in packets.iter().enumerate() {
            if packets
                .get(i + 1)
                .is_some_and(|next| next.pts != p.pts + p.duration)
            {
                gaps += 1;
            }
            file.extend_from_slice(
                &aac::adts_header(48_000, 2, p.len()).ok_or("packet too large")?,
            );
            file.extend_from_slice(&p.data);
        }
        let path = format!("{out}-{track}.aac");
        std::fs::write(&path, &file)?;
        if let (Some(first), Some(last)) = (packets.first(), packets.last()) {
            println!(
                "track {track}: {} packets, {} bytes, timestamp gaps: {gaps}, {:.3} s (first pts {:+.3} s after start, end {:+.3} s vs stop) -> {path}",
                packets.len(),
                file.len(),
                as_secs_f64(last.pts + last.duration - first.pts),
                as_secs_f64(first.pts - start_qpc),
                as_secs_f64(last.pts + last.duration - stop_qpc),
            );
        }
    }
    println!("capture ran {:.3} s", as_secs_f64(stop_qpc - start_qpc));
    // GetProcessTimes is charged per scheduler tick (15.6 ms), so short runs are
    // noisy; QueryProcessCycleTime counts TSC cycles exactly.
    let cycle_secs = cycles as f64 / tsc_hz;
    println!(
        "cpu (cycles): {:.1} ms over {:.1} s = {:.3}% of one core",
        cycle_secs * 1e3,
        wall.as_secs_f64(),
        100.0 * cycle_secs / wall.as_secs_f64()
    );
    println!(
        "cpu (GetProcessTimes): {:.1} ms = {:.3}% of one core",
        cpu.as_secs_f64() * 1e3,
        100.0 * cpu.as_secs_f64() / wall.as_secs_f64()
    );
    Ok(())
}

/// User + kernel CPU time of this process.
#[cfg(windows)]
fn cpu_time() -> std::time::Duration {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    let mut times = [FILETIME::default(); 4];
    let [creation, exit, kernel, user] = &mut times;
    // SAFETY: the pseudo handle is always valid; all out pointers are valid.
    if unsafe { GetProcessTimes(GetCurrentProcess(), creation, exit, kernel, user) }.is_err() {
        return std::time::Duration::ZERO;
    }
    let ticks = |t: &FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    std::time::Duration::from_nanos((ticks(kernel) + ticks(user)) * 100)
}

/// CPU cycles used by this process (TSC units).
#[cfg(windows)]
fn cycles() -> u64 {
    use windows::Win32::System::Threading::GetCurrentProcess;
    use windows::Win32::System::WindowsProgramming::QueryProcessCycleTime;

    let mut cycles = 0;
    // SAFETY: the pseudo handle is always valid; the out pointer is valid.
    let _ = unsafe { QueryProcessCycleTime(GetCurrentProcess(), &mut cycles) };
    cycles
}

#[cfg(windows)]
fn tsc() -> u64 {
    // SAFETY: RDTSC is available on every x86-64 CPU.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::x86_64::_rdtsc()
    }
    #[cfg(not(target_arch = "x86_64"))]
    0
}
