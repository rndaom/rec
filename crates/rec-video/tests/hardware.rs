//! End-to-end checks against the real GPU, capture and encoder. They need a
//! desktop session with a hardware encoder, so they are ignored by default:
//!
//! ```text
//! cargo test --release -p rec-video --test hardware -- --ignored --nocapture --test-threads=1
//! ```
#![cfg(windows)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rec_video::{
    CaptureSource, CodecPreference, Packet, VideoCapture, VideoCaptureConfig, VideoCodec, annexb,
};
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFStartup};
use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Threading::GetCurrentProcess;

fn mf_startup() {
    // SAFETY: MFStartup may be called more than once per process; the
    // matching MFShutdown is skipped because tests share the process.
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.expect("MFStartup");
}

fn private_bytes() -> usize {
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
    m.PrivateUsage
}

fn record(codec: CodecPreference, duration: Duration) -> (VideoCodec, Vec<Packet>) {
    let packets = Arc::new(Mutex::new(Vec::new()));
    let sink_packets = packets.clone();
    let config = VideoCaptureConfig {
        codec,
        max_height: Some(720),
        ..VideoCaptureConfig::new(CaptureSource::PrimaryMonitor)
    };
    let capture = VideoCapture::start(
        config,
        Box::new(move |p| sink_packets.lock().unwrap().push(p)),
    )
    .expect("start");
    let codec = capture.format().codec;
    std::thread::sleep(duration);
    let stats = capture.stats();
    assert!(stats.running, "{stats:?}");
    assert_eq!(stats.last_error, None);
    capture.stop();
    let packets = std::mem::take(&mut *packets.lock().unwrap());
    (codec, packets)
}

fn check_contract(codec: VideoCodec, packets: &[Packet]) {
    assert!(!packets.is_empty());
    assert!(packets[0].keyframe, "stream must start with a keyframe");
    for w in packets.windows(2) {
        assert!(w[1].pts > w[0].pts, "pts must increase (no B-frames)");
    }
    for p in packets {
        assert_eq!(p.pts, p.dts);
        assert!(p.duration > 0);
        let first = annexb::first_vcl_type(codec, &p.data).expect("a slice");
        assert_eq!(p.keyframe, annexb::is_idr(codec, first));
        if p.keyframe {
            assert!(annexb::has_parameter_sets(codec, &p.data));
        }
    }
    let keys: Vec<i64> = packets
        .iter()
        .filter(|p| p.keyframe)
        .map(|p| p.pts)
        .collect();
    for w in keys.windows(2) {
        let gap = w[1] - w[0];
        assert!(
            (9_000_000..=11_000_000).contains(&gap),
            "keyframe gap {gap}"
        );
    }
}

#[test]
#[ignore = "needs a GPU, a desktop session and a hardware encoder"]
fn hevc_and_h264_follow_the_packet_contract() {
    mf_startup();
    for pref in [CodecPreference::Hevc, CodecPreference::H264] {
        let (codec, packets) = record(pref, Duration::from_millis(2500));
        println!("{codec:?}: {} packets", packets.len());
        check_contract(codec, &packets);
    }
}

#[test]
#[ignore = "needs a GPU, a desktop session and a hardware encoder"]
fn repeated_start_stop_does_not_leak() {
    mf_startup();
    // Warm up: driver and MF heaps grow over the first cycles, then settle
    // (measured on AMD: ~+12 MB over the first 10-20 cycles, then
    // ~50 KB/cycle of driver/MF heap noise; our own objects are all freed).
    for _ in 0..10 {
        let _ = record(CodecPreference::Auto, Duration::from_millis(300));
    }
    let before = private_bytes();
    let cycles = 20;
    for _ in 0..cycles {
        let (_, packets) = record(CodecPreference::Auto, Duration::from_millis(300));
        assert!(!packets.is_empty());
    }
    let after = private_bytes();
    let growth = after as i64 - before as i64;
    println!(
        "private bytes {:.1} MB -> {:.1} MB ({:+.1} MB over {cycles} cycles)",
        before as f64 / 1048576.0,
        after as f64 / 1048576.0,
        growth as f64 / 1048576.0
    );
    assert!(
        growth < 16 << 20,
        "grew {growth} bytes over {cycles} start/stop cycles"
    );
}
