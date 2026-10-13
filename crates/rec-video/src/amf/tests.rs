//! Unit tests, plus hardware tests that need an AMD GPU with the AMF runtime
//! (and ffmpeg/ffprobe on PATH for the bitstream checks):
//!
//! ```text
//! cargo test --release -p rec-video amf -- --ignored --nocapture
//! ```

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use rec_core::media::{HdrMetadata, VideoCodec, VideoColor};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_WRITE,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAP_WRITE,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::core::{IUnknown, Interface};

use super::*;
use crate::annexb;

const FPS: u32 = 60;
const FRAMES: usize = 180;
const KBPS: u32 = 12_000;
/// Inputs in flight at most; a slot is reused only after its tag came back.
const POOL: usize = 8;
const DUR: Hns = 10_000_000 / FPS as Hns;
/// A QPC-like start time, as the pipeline would use.
const BASE_PTS: Hns = 123_456_789_000;

#[test]
fn layout_helpers() {
    assert_eq!(format_version(sys::make_version(1, 4, 36, 7)), "1.4.36");
    assert_eq!(result_name(sys::AMF_INPUT_FULL), "AMF_INPUT_FULL");
    assert_eq!(result_name(sys::AMF_NEED_MORE_INPUT), "AMF_NEED_MORE_INPUT");
    assert_eq!(result_name(-3), "AMF_RESULT -3");
    assert_eq!(Variant::int(-5).as_i64(), Some(-5));
    assert_eq!(Variant::bool(true).as_i64(), Some(1));
    assert_eq!(Variant::size(2, 3).as_i64(), None);
}

#[test]
fn peak_is_one_and_a_half_times_mean() {
    assert_eq!(bitrate_bps(20_000), (20_000_000, 30_000_000));
    assert_eq!(bitrate_bps(1), (1_000, 1_500));
    let (mean, peak) = bitrate_bps(u32::MAX);
    assert!(peak >= mean);
}

#[test]
fn rejects_h264_hdr_and_bad_sizes() {
    let p = |codec, width, color| AmfParams {
        codec,
        width,
        height: 720,
        fps: 60,
        bitrate_kbps: 8000,
        color,
    };
    let hdr = VideoColor::Hdr10(HdrMetadata::bt2020_1000());
    assert!(matches!(
        validate(&p(VideoCodec::H264, 1280, hdr)),
        Err(VideoError::InvalidConfig(_))
    ));
    assert!(validate(&p(VideoCodec::Hevc, 1280, hdr)).is_ok());
    assert!(validate(&p(VideoCodec::Av1, 1281, VideoColor::Sdr)).is_err());
    assert!(validate(&p(VideoCodec::H264, 1280, VideoColor::Sdr)).is_ok());
}

#[test]
fn rate_control_names_follow_each_header() {
    assert_eq!(rate_control_name(VideoCodec::H264, 1), "CBR");
    assert_eq!(
        rate_control_name(VideoCodec::H264, 3),
        "latency-constrained VBR"
    );
    assert_eq!(rate_control_name(VideoCodec::Hevc, 3), "CBR");
    assert_eq!(
        rate_control_name(VideoCodec::Av1, 2),
        "peak-constrained VBR"
    );
}

/// Never panics; `None` simply means no AMD driver.
#[test]
fn runtime_version_does_not_panic() {
    println!("AMF runtime: {:?}", runtime_version());
}

// ---------------------------------------------------------------------------
// Hardware

fn amd_device() -> (ID3D11Device, ID3D11DeviceContext) {
    // SAFETY: plain factory creation.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.expect("DXGI factory");
    for i in 0.. {
        // SAFETY: EnumAdapters1 errs past the end.
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
            break;
        };
        // SAFETY: fills a plain struct.
        let desc = unsafe { adapter.GetDesc1() }.expect("GetDesc1");
        if desc.VendorId != 0x1002 {
            continue;
        }
        let (mut device, mut context) = (None, None);
        // SAFETY: valid adapter and out-pointers; as `d3d::create_for_source`.
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .expect("D3D11CreateDevice");
        let (device, context) = (device.unwrap(), context.unwrap());
        let mt: ID3D11Multithread = context.cast().unwrap();
        // SAFETY: flag set on a valid interface, as the pipeline does.
        let _ = unsafe { mt.SetMultithreadProtected(true) };
        return (device, context);
    }
    panic!("no AMD adapter");
}

fn texture(
    device: &ID3D11Device,
    format: DXGI_FORMAT,
    w: u32,
    h: u32,
    staging: bool,
) -> ID3D11Texture2D {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: if staging {
            D3D11_USAGE_STAGING
        } else {
            D3D11_USAGE_DEFAULT
        },
        // As the pipeline's converter output: render target + shader resource.
        BindFlags: if staging {
            0
        } else {
            (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32
        },
        CPUAccessFlags: if staging {
            D3D11_CPU_ACCESS_WRITE.0 as u32
        } else {
            0
        },
        MiscFlags: 0,
    };
    let mut tex = None;
    // SAFETY: valid description and out-pointer.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.expect("CreateTexture2D");
    tex.unwrap()
}

/// Draws frame `n` of a moving test pattern into `staging`: scrolling
/// diagonal bars plus a 384x216 block of per-frame noise, so the rate
/// control has real work without hitting the codec's quality floor.
fn draw(
    context: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    hdr: bool,
    (w, h): (u32, u32),
    n: usize,
) {
    let mut map = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: CPU-writable staging texture; unmapped below.
    unsafe { context.Map(staging, 0, D3D11_MAP_WRITE, 0, Some(&mut map)) }.expect("Map");
    let (w, h, pitch) = (w as usize, h as usize, map.RowPitch as usize);
    // SAFETY: a mapped planar 4:2:0 texture is `h` luma rows then `h / 2`
    // interleaved chroma rows, each `pitch` bytes.
    let bytes =
        unsafe { std::slice::from_raw_parts_mut(map.pData.cast::<u8>(), pitch * (h + h / 2)) };
    let mut seed = (n as u32).wrapping_mul(2_654_435_761) | 1;
    let mut noise = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    let (nx, ny) = (w / 2, h / 2);
    // Writes an 8-bit studio-range level, as P010 (10 bits in the high
    // bits of 16) for HDR.
    let put = |row: &mut [u8], i: usize, v: u32| {
        if hdr {
            row[2 * i..2 * i + 2].copy_from_slice(&(((v * 4) as u16) << 6).to_le_bytes());
        } else {
            row[i] = v as u8;
        }
    };
    for y in 0..h {
        let row = &mut bytes[y * pitch..];
        for x in 0..w {
            let v = if (nx..nx + 384).contains(&x) && (ny..ny + 216).contains(&y) {
                16 + noise() % 220
            } else {
                16 + ((x + y / 4 + n * 12) % 256 * 219 / 255) as u32
            };
            put(row, x, v);
        }
    }
    for y in 0..h / 2 {
        let row = &mut bytes[(h + y) * pitch..];
        for x in 0..w / 2 {
            put(row, 2 * x, 128 + ((x / 32 + n) % 64) as u32);
            put(row, 2 * x + 1, 128 - ((y / 32) % 64) as u32);
        }
    }
    // SAFETY: balances the Map.
    unsafe { context.Unmap(staging, 0) };
}

struct Output {
    data: Vec<u8>,
    pts: Hns,
    key: bool,
    /// Time from submit to the poll that returned it.
    latency: Duration,
    /// Frames submitted after this one before it came out.
    latency_frames: usize,
}

struct Run {
    outputs: Vec<Output>,
    create: Duration,
    create_again: Duration,
    max_in_flight: usize,
    input_full: usize,
    /// What `set_bitrate` returned, if it was called.
    bitrate_after: Option<u32>,
    max_draw: Duration,
    name: String,
    rate_control: String,
}

fn params(codec: VideoCodec, hdr: bool, (w, h): (u32, u32)) -> AmfParams {
    AmfParams {
        codec,
        width: w,
        height: h,
        fps: FPS,
        bitrate_kbps: KBPS,
        color: if hdr {
            VideoColor::Hdr10(HdrMetadata::bt2020_1000())
        } else {
            VideoColor::Sdr
        },
    }
}

/// Encodes `frames` frames in real time from a pool of POOL textures that
/// are reused only once their tags come back, checking release tracking on
/// the way. With `change_rate`, the bit rate drops to a quarter half-way.
fn encode(codec: VideoCodec, hdr: bool, size: (u32, u32), frames: usize, change_rate: bool) -> Run {
    let (device, context) = amd_device();
    let params = params(codec, hdr, size);
    let t = Instant::now();
    let first = AmfEncoder::new(&device, &params).expect("AmfEncoder::new");
    let create = t.elapsed();
    drop(first);
    let t = Instant::now();
    let mut enc = AmfEncoder::new(&device, &params).expect("AmfEncoder::new (again)");
    let create_again = t.elapsed();
    assert_eq!(enc.codec(), codec);
    assert_eq!(enc.coded_size(), size);

    let format = enc.format;
    let pool: Vec<_> = (0..POOL)
        .map(|_| texture(&device, format, size.0, size.1, false))
        .collect();
    let staging = texture(&device, format, size.0, size.1, true);
    let mut free: VecDeque<usize> = (0..POOL).collect();
    let mut busy = [false; POOL];
    let mut released = 0usize;
    let mut outputs: Vec<Output> = Vec::new();
    let mut submitted_at = Vec::new();
    let mut input_full = 0;
    let mut max_in_flight = 0;
    let mut bitrate_after = None;
    let mut max_draw = Duration::ZERO;

    let poll = |enc: &mut AmfEncoder,
                free: &mut VecDeque<usize>,
                busy: &mut [bool; POOL],
                released: &mut usize,
                outputs: &mut Vec<Output>,
                submitted_at: &[Instant]| {
        enc.poll(
            |data, pts, key| {
                let i = ((pts - BASE_PTS) / DUR) as usize;
                outputs.push(Output {
                    data: data.to_vec(),
                    pts,
                    key,
                    latency: submitted_at[i].elapsed(),
                    latency_frames: submitted_at.len() - 1 - i,
                });
            },
            |tag| {
                assert!(busy[tag], "tag {tag} released twice or never submitted");
                busy[tag] = false;
                *released += 1;
                free.push_back(tag);
            },
        )
        .expect("poll");
    };

    let start = Instant::now();
    for n in 0..frames {
        // Real-time pacing at FPS.
        let due = start + Duration::from_nanos(n as u64 * 1_000_000_000 / u64::from(FPS));
        while Instant::now() < due {
            poll(
                &mut enc,
                &mut free,
                &mut busy,
                &mut released,
                &mut outputs,
                &submitted_at,
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        if change_rate && n == frames / 2 {
            let t = Instant::now();
            let now = enc.set_bitrate(KBPS / 4).expect("set_bitrate");
            println!("  set_bitrate({}) -> {now} in {:?}", KBPS / 4, t.elapsed());
            bitrate_after = Some(now);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        let slot = loop {
            if let Some(s) = free.pop_front() {
                break s;
            }
            assert!(
                Instant::now() < deadline,
                "no input texture came back: frame {n}, {} outputs, {released} released",
                outputs.len()
            );
            poll(
                &mut enc,
                &mut free,
                &mut busy,
                &mut released,
                &mut outputs,
                &submitted_at,
            );
            std::thread::sleep(Duration::from_micros(500));
        };
        let t = Instant::now();
        draw(&context, &staging, hdr, size, n);
        max_draw = max_draw.max(t.elapsed());
        // SAFETY: same-format, same-size resources on one device.
        unsafe { context.CopyResource(&pool[slot], &staging) };
        busy[slot] = true;
        submitted_at.push(Instant::now());
        let pts = BASE_PTS + n as Hns * DUR;
        while !enc.submit(&pool[slot], slot, pts, DUR).expect("submit") {
            input_full += 1;
            assert!(Instant::now() < deadline, "input queue stayed full");
            poll(
                &mut enc,
                &mut free,
                &mut busy,
                &mut released,
                &mut outputs,
                &submitted_at,
            );
            std::thread::sleep(Duration::from_micros(500));
        }
        max_in_flight = max_in_flight.max(busy.iter().filter(|b| **b).count());
        poll(
            &mut enc,
            &mut free,
            &mut busy,
            &mut released,
            &mut outputs,
            &submitted_at,
        );
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while (outputs.len() < frames || released < frames) && Instant::now() < deadline {
        poll(
            &mut enc,
            &mut free,
            &mut busy,
            &mut released,
            &mut outputs,
            &submitted_at,
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        released, frames,
        "every submitted tag released exactly once"
    );
    assert!(busy.iter().all(|b| !b));
    let (name, rate_control) = (enc.name().to_string(), enc.rate_control().to_string());
    drop(enc);
    Run {
        outputs,
        create,
        create_again,
        max_in_flight,
        input_full,
        bitrate_after,
        max_draw,
        name,
        rate_control,
    }
}

/// Checks the packet contract on the encoder's own output.
fn check_stream(codec: VideoCodec, run: &Run, frames: usize) {
    let out = &run.outputs;
    assert_eq!(out.len(), frames, "one output per input");
    assert!(out[0].key, "first output must be a keyframe");
    for (i, o) in out.iter().enumerate() {
        assert_eq!(
            o.pts,
            BASE_PTS + i as Hns * DUR,
            "output {i}: pts passes through"
        );
        assert_eq!(
            o.key,
            i % FPS as usize == 0,
            "output {i}: keyframe every {FPS} frames"
        );
        match codec {
            VideoCodec::H264 | VideoCodec::Hevc => {
                let first = annexb::first_vcl_type(codec, &o.data).expect("a slice");
                assert_eq!(annexb::is_idr(codec, first), o.key, "output {i}: IDR flag");
                if o.key {
                    assert!(
                        annexb::has_parameter_sets(codec, &o.data),
                        "output {i}: parameter sets"
                    );
                }
            }
            VideoCodec::Av1 => check_temporal_unit(&o.data, o.key, i),
        }
    }
}

/// One AV1 temporal unit: every OBU has a size field, exactly one shown
/// frame (no hidden or show-existing frames), key frame iff `key`, and a
/// sequence header on key frames.
fn check_temporal_unit(mut d: &[u8], key: bool, i: usize) {
    let (mut frames, mut seq) = (0, false);
    while !d.is_empty() {
        let header = d[0];
        assert_eq!(header >> 7, 0, "output {i}: forbidden bit");
        let kind = (header >> 3) & 0xF;
        let ext = (header >> 2) & 1;
        assert_eq!((header >> 1) & 1, 1, "output {i}: OBU without size field");
        let mut pos = 1 + ext as usize;
        let mut size = 0usize;
        for k in 0.. {
            let b = d[pos];
            pos += 1;
            size |= usize::from(b & 0x7F) << (7 * k);
            if b & 0x80 == 0 {
                break;
            }
        }
        let payload = &d[pos..pos + size];
        match kind {
            1 => seq = true,
            3 | 6 => {
                frames += 1;
                // uncompressed_header (no reduced still picture header):
                // show_existing_frame f(1), frame_type f(2), show_frame f(1).
                let b = payload[0];
                assert_eq!(b >> 7, 0, "output {i}: show_existing_frame");
                assert_eq!(
                    (b >> 5) & 3 == 0,
                    key,
                    "output {i}: frame_type vs keyframe flag"
                );
                assert_eq!((b >> 4) & 1, 1, "output {i}: hidden frame");
            }
            _ => {}
        }
        d = &d[pos + size..];
    }
    assert_eq!(frames, 1, "output {i}: one frame per temporal unit");
    if key {
        assert!(seq, "output {i}: key frame without sequence header");
    }
}

/// Removes the file on drop, pass or fail.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The elementary stream as a file ffmpeg reads: Annex-B as is, AV1 in IVF.
fn write_stream(codec: VideoCodec, label: &str, (w, h): (u32, u32), run: &Run) -> TempFile {
    let ext = match codec {
        VideoCodec::H264 => "h264",
        VideoCodec::Hevc => "hevc",
        VideoCodec::Av1 => "ivf",
    };
    let path = std::env::temp_dir().join(format!(
        "rec-amf-test-{}-{}-{label}-{w}x{h}.{ext}",
        std::process::id(),
        codec.name().replace('.', ""),
    ));
    let mut bytes = Vec::new();
    if codec == VideoCodec::Av1 {
        // IVF: 32-byte file header, then a 12-byte header per frame.
        bytes.extend_from_slice(b"DKIF");
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&32u16.to_le_bytes());
        bytes.extend_from_slice(b"AV01");
        bytes.extend_from_slice(&(w as u16).to_le_bytes());
        bytes.extend_from_slice(&(h as u16).to_le_bytes());
        bytes.extend_from_slice(&FPS.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&(run.outputs.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        for (i, o) in run.outputs.iter().enumerate() {
            bytes.extend_from_slice(&(o.data.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&(i as u64).to_le_bytes());
            bytes.extend_from_slice(&o.data);
        }
    } else {
        for o in &run.outputs {
            bytes.extend_from_slice(&o.data);
        }
    }
    std::fs::write(&path, bytes).expect("write stream");
    TempFile(path)
}

/// Decodes the whole file with ffmpeg (no errors allowed) and returns
/// ffprobe's stream description as `key=value` lines.
fn ffmpeg_check(file: &TempFile) -> String {
    let decode = Command::new("ffmpeg")
        .args(["-hide_banner", "-v", "error", "-i"])
        .arg(&file.0)
        .args(["-f", "null", "-"])
        .output()
        .expect("run ffmpeg");
    let errors = String::from_utf8_lossy(&decode.stderr);
    assert!(
        decode.status.success() && errors.trim().is_empty(),
        "ffmpeg decode errors:\n{errors}"
    );
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_frames",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,profile,pix_fmt,width,height,sample_aspect_ratio,color_range,\
             color_space,color_transfer,color_primaries,nb_read_frames",
            "-of",
            "default=nw=1",
        ])
        .arg(&file.0)
        .output()
        .expect("run ffprobe");
    assert!(probe.status.success());
    String::from_utf8_lossy(&probe.stdout).replace("\r\n", "\n")
}

fn assert_probe(probe: &str, lines: &[String]) {
    for line in lines {
        assert!(
            probe.lines().any(|l| l == line),
            "ffprobe lacks {line}:\n{probe}"
        );
    }
}

/// Bit rate in kbit/s over a range of outputs.
fn kbps(run: &Run, range: std::ops::Range<usize>) -> usize {
    let bytes: usize = run.outputs[range.clone()]
        .iter()
        .map(|o| o.data.len())
        .sum();
    bytes * 8 * FPS as usize / range.len() / 1000
}

/// The full check for one codec / colour: ~3 s at 60 fps, a live bit-rate
/// change half-way, the packet contract, release tracking, and ffmpeg /
/// ffprobe on the elementary stream.
fn hardware_case(codec: VideoCodec, hdr: bool, size: (u32, u32), profile: &str) {
    println!("AMF runtime {:?}", runtime_version());
    let run = encode(codec, hdr, size, FRAMES, true);
    check_stream(codec, &run, FRAMES);

    let latency = run.outputs.iter().map(|o| o.latency.as_secs_f64() * 1e3);
    let mean_latency = latency.sum::<f64>() / run.outputs.len() as f64;
    let max_frames = run
        .outputs
        .iter()
        .map(|o| o.latency_frames)
        .max()
        .unwrap_or(0);
    let keys: Vec<usize> = (0..run.outputs.len())
        .filter(|&i| run.outputs[i].key)
        .collect();
    let (before, after) = (kbps(&run, 30..90), kbps(&run, 120..180));
    println!(
        "{}: {}\n  created in {:?} (again {:?}); keyframes at {keys:?}; output latency mean \
         {mean_latency:.2} ms, max {max_frames} frame(s); max textures in flight {}; \
         input-full retries {}; max draw {:?}\n  {KBPS} kbit/s target: {before} kbit/s \
         (frames 30..90); then {:?} kbit/s target: {after} kbit/s (frames 120..180)",
        run.name,
        run.rate_control,
        run.create,
        run.create_again,
        run.max_in_flight,
        run.input_full,
        run.max_draw,
        run.bitrate_after,
    );
    assert_eq!(run.bitrate_after, Some(KBPS / 4));
    assert!(after * 10 < before * 7, "bit rate change had no effect");

    let file = write_stream(codec, if hdr { "hdr" } else { "sdr" }, size, &run);
    let probe = ffmpeg_check(&file);
    println!("  ffprobe:\n    {}", probe.trim().replace('\n', "\n    "));
    let (pix, space, transfer, primaries) = if hdr {
        ("yuv420p10le", "bt2020nc", "smpte2084", "bt2020")
    } else {
        ("yuv420p", "bt709", "bt709", "bt709")
    };
    assert_probe(
        &probe,
        &[
            format!("profile={profile}"),
            format!("pix_fmt={pix}"),
            format!("width={}", size.0),
            format!("height={}", size.1),
            "color_range=tv".into(),
            format!("color_space={space}"),
            format!("color_transfer={transfer}"),
            format!("color_primaries={primaries}"),
            format!("nb_read_frames={FRAMES}"),
        ],
    );
}

const HD: (u32, u32) = (1920, 1080);
/// AV1 needs 64 x 16 multiples on RDNA3 (see `size_alignment`).
const QHD: (u32, u32) = (2560, 1440);

#[test]
#[ignore = "needs an AMD GPU with the AMF runtime, ffmpeg and ffprobe"]
fn amf_h264_sdr() {
    hardware_case(VideoCodec::H264, false, HD, "High");
}

#[test]
#[ignore = "needs an AMD GPU with the AMF runtime, ffmpeg and ffprobe"]
fn amf_hevc_sdr() {
    hardware_case(VideoCodec::Hevc, false, HD, "Main");
}

#[test]
#[ignore = "needs an AMD GPU with the AMF runtime, ffmpeg and ffprobe"]
fn amf_hevc_hdr10() {
    hardware_case(VideoCodec::Hevc, true, HD, "Main 10");
}

#[test]
#[ignore = "needs an AMD GPU with the AMF runtime, ffmpeg and ffprobe"]
fn amf_av1_sdr() {
    hardware_case(VideoCodec::Av1, false, QHD, "Main");
}

#[test]
#[ignore = "needs an AMD GPU with the AMF runtime, ffmpeg and ffprobe"]
fn amf_av1_hdr10() {
    hardware_case(VideoCodec::Av1, true, QHD, "Main");
}

/// Coded sizes are exact: H.264 / HEVC at an odd window size, AV1 at
/// aligned sizes; AV1 sizes the hardware would pad are refused.
#[test]
#[ignore = "needs an AMD GPU with the AMF runtime, ffmpeg and ffprobe"]
fn amf_sizes() {
    let (device, _context) = amd_device();
    assert_eq!(
        size_alignment(&device, VideoCodec::Av1).expect("alignment"),
        (64, 16)
    );
    assert_eq!(
        size_alignment(&device, VideoCodec::Hevc).expect("alignment"),
        (2, 2)
    );
    for size in [HD, (946, 532)] {
        let r = AmfEncoder::new(&device, &params(VideoCodec::Av1, false, size));
        println!("AV1 {size:?}: {:?}", r.as_ref().map(|_| ()));
        assert!(matches!(r, Err(VideoError::InvalidConfig(_))));
    }
    for (codec, size) in [
        (VideoCodec::H264, (946, 532)),
        (VideoCodec::Hevc, (946, 532)),
        (VideoCodec::Av1, (1920, 1088)),
        (VideoCodec::Av1, QHD),
    ] {
        let frames = 2 * FPS as usize;
        let run = encode(codec, false, size, frames, false);
        check_stream(codec, &run, frames);
        let file = write_stream(codec, "size", size, &run);
        let probe = ffmpeg_check(&file);
        assert_probe(
            &probe,
            &[
                format!("width={}", size.0),
                format!("height={}", size.1),
                "sample_aspect_ratio=1:1".into(),
                format!("nb_read_frames={frames}"),
            ],
        );
        println!("{codec:?} {size:?}: exact, {frames} frames decode cleanly");
    }
}

fn refcount(t: &ID3D11Texture2D) -> u32 {
    let u: IUnknown = t.cast().unwrap();
    // SAFETY: a balanced AddRef/Release pair; Release returns the count.
    unsafe {
        (u.vtable().AddRef)(u.as_raw());
        (u.vtable().Release)(u.as_raw())
    }
}

/// Submitting without polling fills the input queue: the refused frame is
/// not reported, and dropping the encoder with frames in flight releases
/// every texture reference it held.
#[test]
#[ignore = "needs an AMD GPU with the AMF runtime"]
fn amf_input_full_and_drop_in_flight() {
    let (device, _context) = amd_device();
    let params = AmfParams {
        codec: VideoCodec::Hevc,
        width: 640,
        height: 360,
        fps: 60,
        bitrate_kbps: 4000,
        color: VideoColor::Sdr,
    };
    let mut enc = AmfEncoder::new(&device, &params).expect("new");
    let textures: Vec<_> = (0..64)
        .map(|_| texture(&device, DXGI_FORMAT_NV12, 640, 360, false))
        .collect();
    let baseline: Vec<u32> = textures.iter().map(refcount).collect();
    let mut accepted = 0;
    for (i, t) in textures.iter().enumerate() {
        if !enc.submit(t, i, i as Hns * DUR, DUR).expect("submit") {
            break;
        }
        accepted += 1;
    }
    println!("input queue took {accepted} frames before AMF_INPUT_FULL");
    assert!(accepted < textures.len(), "queue never filled");
    let refused = accepted;
    let mut released = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut outputs = 0;
    while released.len() < accepted / 2 && Instant::now() < deadline {
        enc.poll(|_, _, _| outputs += 1, |tag| released.push(tag))
            .expect("poll");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!released.contains(&refused), "refused frame reported");
    println!(
        "{outputs} outputs, {} released while draining",
        released.len()
    );
    // Queue more, then drop with them in flight.
    for (i, t) in textures.iter().enumerate().take(refused + 4).skip(refused) {
        let _ = enc.submit(t, i, i as Hns * DUR, DUR).expect("submit");
    }
    drop(enc);
    let after: Vec<u32> = textures.iter().map(refcount).collect();
    assert_eq!(after, baseline, "texture references left behind");
}

#[test]
#[ignore = "needs an AMD GPU with the AMF runtime"]
fn amf_rejects_wrong_texture() {
    let (device, _context) = amd_device();
    let params = AmfParams {
        codec: VideoCodec::H264,
        width: 640,
        height: 360,
        fps: 30,
        bitrate_kbps: 2000,
        color: VideoColor::Sdr,
    };
    let mut enc = AmfEncoder::new(&device, &params).expect("new");
    let wrong = texture(&device, DXGI_FORMAT_NV12, 320, 180, false);
    assert!(matches!(
        enc.submit(&wrong, 0, 0, DUR),
        Err(VideoError::InvalidConfig(_))
    ));
}
