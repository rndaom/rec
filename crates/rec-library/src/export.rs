//! "Discord-ready" export: re-encodes a clip to an H.264 + AAC MP4 that fits
//! under a size limit (Discord's free upload limit is 10 MB; the UI also
//! offers 50 and 500 MB, see [`PRESETS`]).
//!
//! Media Foundation only (D4): the source reader decodes on the GPU and its
//! video processor scales to NV12, the sink writer encodes with the hardware
//! H.264 encoder from the same D3D11 device, so frames stay on the GPU. If
//! that fails, frames are copied to system memory for whichever encoder the
//! sink writer finds. Audio is one of the clip's tracks (track 0 is the mix)
//! decoded to PCM and re-encoded as AAC-LC.
//!
//! The bitrate comes from the limit and the duration ([`plan`]). Encoders
//! don't hit a bitrate exactly, so a file over the limit is encoded again
//! with the bitrate scaled down by the overshoot, at most [`MAX_ATTEMPTS`]
//! times in all. The index (`moov`) is then moved to the front of the file
//! so playback can start before the whole file has downloaded.
//!
//! Known limitation: HDR10 clips come out as SDR BT.709 8-bit through the
//! video processor's own HDR-to-SDR conversion (the driver's), which on
//! AMD (RX 7800 XT) is clearly too dark and flat: mean luma about half of
//! the same footage recorded as SDR, with lifted blacks. Usable, not good.
//! The fix is rec-video's tone-map shader (`convert.hlsl`) applied to the
//! decoded P010 texture, moved somewhere both crates can use; rec-library
//! doesn't depend on rec-video. [`Exported::from_hdr`] lets the UI say so.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMaxBitRate, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonQualityVsSpeed, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVGOPSize, IMF2DBuffer, IMFAttributes, IMFDXGIDeviceManager, IMFMediaType,
    IMFSample, IMFSinkWriter, IMFSourceReader, MF_MT_AAC_PAYLOAD_TYPE,
    MF_MT_ALL_SAMPLES_INDEPENDENT, MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE,
    MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
    MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_MPEG2_PROFILE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE,
    MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MF_PD_DURATION,
    MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SINK_WRITER_D3D_MANAGER,
    MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_D3D_MANAGER,
    MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READER_MEDIASOURCE, MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR,
    MF_TRANSCODE_CONTAINERTYPE, MFAudioFormat_AAC, MFAudioFormat_PCM, MFCreateAttributes,
    MFCreateDXGIDeviceManager, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFCreateSinkWriterFromURL, MFCreateSourceReaderFromURL, MFMediaType_Audio, MFMediaType_Video,
    MFNominalRange_16_235, MFTranscodeContainerType_MPEG4, MFVideoArea, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFVideoPrimaries_BT709, MFVideoTransFunc_709,
    MFVideoTransFunc_2084, MFVideoTransferMatrix_BT709,
    eAVEncCommonRateControlMode_PeakConstrainedVBR, eAVEncH264VProfile_High,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::core::{GUID, HSTRING, Interface};

use crate::decode::{create_device, ctx};

/// Upload limits the UI offers, in bytes: 10, 50 and 500 MB. Decimal
/// megabytes, a little under Discord's binary ones (10 MiB free).
pub const PRESETS: [u64; 3] = [10_000_000, 50_000_000, 500_000_000];
/// The default limit: Discord's free upload limit.
pub const DEFAULT_LIMIT: u64 = PRESETS[0];
/// The error [`export`] returns when cancelled.
pub const CANCELLED: &str = "cancelled";
/// Encodes per export, counting retries after a file came out too big.
pub const MAX_ATTEMPTS: u32 = 3;

/// The share of the limit spent on audio and video; the rest is margin for
/// the container (an MP4's index is ~0.5% of it) and encoder overshoot.
const PAYLOAD: f64 = 0.96;
/// After an overshoot the budget is scaled by `limit / size` and this.
const RETRY_MARGIN: f64 = 0.95;
/// Bits per pixel below which H.264 game footage falls apart: the picture
/// is made smaller (then the frame rate lower) to stay above it.
const MIN_BPP: f64 = 0.06;
/// Bits per pixel above which more bitrate buys little: big limits give
/// smaller files rather than wasted bits. 1080p60 gets 12.4 Mbps, about
/// YouTube's recommended upload rate (a 50 MB export of an 18 Mbps HEVC
/// clip at 0.15 came out bigger than the clip).
const MAX_BPP: f64 = 0.1;
/// Output heights tried below the source's, largest first.
const HEIGHTS: [u32; 7] = [2160, 1440, 1080, 720, 540, 480, 360];
/// The frame rate cap, and the cap when the budget is tight.
const MAX_FPS: u32 = 60;
const TIGHT_FPS: u32 = 30;
/// Below this video bitrate a clip can't be exported watchably.
const MIN_VIDEO_KBPS: u32 = 150;
/// AAC bitrates. The Microsoft AAC encoder takes only 96, 128, 160 and 192
/// kbps, so 96 is the floor.
const AUDIO_KBPS: u32 = 128;
const TIGHT_AUDIO_KBPS: u32 = 96;
/// Total bitrates below this get the lower audio bitrate.
const TIGHT_TOTAL_KBPS: f64 = 1000.0;
/// Progress callbacks at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

const HNS: f64 = 10_000_000.0;
const VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
/// Timestamps closer than this count as equal.
const EPSILON: i64 = 10_000;
/// PCM out of the reader: 16-bit stereo at 48 kHz (rec records that, so
/// the reader only decodes; other sources go through its resampler).
const RATE: u32 = 48_000;
const CHANNELS: u32 = 2;

#[derive(Clone, Debug, PartialEq)]
pub struct ExportOptions {
    /// The output must be at most this many bytes.
    pub limit_bytes: u64,
    /// Clip time to start at, in seconds.
    pub start: f64,
    /// Clip time to end at; `None` for the end of the clip.
    pub end: Option<f64>,
    /// Largest output height; the source's when `None`.
    pub max_height: Option<u32>,
    /// Highest output frame rate (60 at most).
    pub max_fps: Option<u32>,
    /// Which audio track: 0 is the mix.
    pub audio_track: usize,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            limit_bytes: DEFAULT_LIMIT,
            start: 0.0,
            end: None,
            max_height: None,
            max_fps: None,
            audio_track: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Progress {
    /// Of the current pass, 0 to 1.
    pub fraction: f64,
    /// The attempt, from 1; a retry starts again from 0.
    pub pass: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Exported {
    pub path: PathBuf,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Seconds of output.
    pub duration: f64,
    /// The video bitrate asked of the encoder on the last attempt.
    pub video_kbps: u32,
    pub audio_kbps: u32,
    pub attempts: u32,
    /// Decoded and encoded on the GPU with frames kept there.
    pub hardware: bool,
    /// The source was HDR10 (PQ), converted to SDR by the video processor.
    pub from_hdr: bool,
}

/// What an export will encode.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plan {
    /// Seconds of output (the trimmed range).
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub video_kbps: u32,
    /// 0 when the clip has no audio.
    pub audio_kbps: u32,
    /// Expected file size, at most the limit.
    pub estimated_bytes: u64,
    /// The video bitrate is high enough to be worth exporting.
    pub fits: bool,
}

/// Chooses size, frame rate and bitrates for a clip of `duration` seconds
/// (the whole clip; the trim range comes from `opts`) whose picture is
/// `src_w` x `src_h` at `src_fps`, assuming it has audio.
pub fn plan(duration: f64, src_w: u32, src_h: u32, src_fps: u32, opts: &ExportOptions) -> Plan {
    let budget = opts.limit_bytes as f64 * PAYLOAD;
    plan_budget(duration, (src_w, src_h), src_fps, opts, budget, true)
}

/// [`plan`] with a byte budget for the audio and video payload.
fn plan_budget(
    clip_duration: f64,
    (src_w, src_h): (u32, u32),
    src_fps: u32,
    opts: &ExportOptions,
    budget: f64,
    audio: bool,
) -> Plan {
    let duration = trimmed(clip_duration, opts).max(0.1);
    let total_kbps = budget * 8.0 / duration / 1000.0;
    let audio_kbps = match audio {
        false => 0,
        true if total_kbps < TIGHT_TOTAL_KBPS => TIGHT_AUDIO_KBPS,
        true => AUDIO_KBPS,
    };
    let mut video_kbps = (total_kbps - f64::from(audio_kbps)).max(0.0);

    let src_w = src_w.max(2);
    let src_h = src_h.max(2);
    let top = src_h.min(opts.max_height.unwrap_or(u32::MAX)).max(2) & !1;
    let heights: Vec<u32> = std::iter::once(top)
        .chain(HEIGHTS.into_iter().filter(|&h| h < top))
        .collect();
    let width_for = |h: u32| {
        let w = (f64::from(src_w) * f64::from(h) / f64::from(src_h)).round() as u32;
        (w.max(2) + 1) & !1
    };
    let bpp = |h: u32, fps: u32| {
        video_kbps * 1000.0 / (f64::from(width_for(h)) * f64::from(h) * f64::from(fps))
    };
    let pick = |fps: u32, lowest: u32| {
        heights
            .iter()
            .copied()
            .find(|&h| h >= lowest && bpp(h, fps) >= MIN_BPP)
    };
    let cap = opts.max_fps.unwrap_or(MAX_FPS).clamp(1, MAX_FPS);
    let mut fps = src_fps.clamp(1, cap);
    let height = if fps > TIGHT_FPS {
        // Keep the full frame rate down to 720p, then halve it.
        pick(fps, 720.min(top)).or_else(|| {
            fps = TIGHT_FPS;
            pick(fps, 0)
        })
    } else {
        pick(fps, 0)
    };
    let height = height.unwrap_or_else(|| {
        fps = fps.min(TIGHT_FPS);
        heights[heights.len() - 1]
    });
    let width = width_for(height);
    let ceiling = MAX_BPP * f64::from(width) * f64::from(height) * f64::from(fps) / 1000.0;
    video_kbps = video_kbps.min(ceiling);
    let video_kbps = video_kbps.floor() as u32;
    let payload = f64::from(video_kbps + audio_kbps) * 1000.0 / 8.0 * duration;
    Plan {
        duration,
        width,
        height,
        fps,
        video_kbps,
        audio_kbps,
        estimated_bytes: (payload * 1.01).min(opts.limit_bytes as f64) as u64,
        fits: video_kbps >= MIN_VIDEO_KBPS,
    }
}

/// Seconds between the trim points, within the clip.
fn trimmed(clip_duration: f64, opts: &ExportOptions) -> f64 {
    let start = opts.start.clamp(0.0, clip_duration);
    let end = opts.end.map_or(clip_duration, |e| e.min(clip_duration));
    (end - start).max(0.0)
}

/// Exports `input` to `output` (an `.mp4`), replacing it. The work runs on
/// the calling thread (put it on an EcoQoS thread: `sys::background`) after
/// [`crate::sys::init`]. `progress` is called at most ~10 times a second.
/// When `cancel` is set the partial file is deleted and the error is
/// [`CANCELLED`].
pub fn export(
    input: &Path,
    output: &Path,
    opts: &ExportOptions,
    progress: &mut dyn FnMut(Progress),
    cancel: &AtomicBool,
) -> Result<Exported, String> {
    let input = std::path::absolute(input).map_err(|e| format!("{}: {e}", input.display()))?;
    let output = std::path::absolute(output).map_err(|e| format!("{}: {e}", output.display()))?;
    let source = Source::probe(&input)?;
    let audio_stream = match source.audio.len() {
        0 => None,
        n if opts.audio_track < n => Some(source.audio[opts.audio_track]),
        n => {
            return Err(format!(
                "the clip has {n} audio tracks, not {}",
                opts.audio_track + 1
            ));
        }
    };
    if opts.start >= source.duration - 0.05 {
        return Err(format!(
            "the start ({:.2} s) is past the end of the clip ({:.2} s)",
            opts.start, source.duration
        ));
    }
    if opts.end.is_some_and(|end| end <= opts.start) {
        return Err("the end is before the start".into());
    }
    let temp = temp_path(&output);
    let mut budget = opts.limit_bytes as f64 * PAYLOAD;
    let fast = fast_path(&output);
    let mut gpu = !force_cpu();
    for attempt in 1..=MAX_ATTEMPTS {
        let plan = plan_budget(
            source.duration,
            (source.width, source.height),
            source.fps,
            opts,
            budget,
            audio_stream.is_some(),
        );
        if !plan.fits {
            let most = budget * 8.0 / f64::from((MIN_VIDEO_KBPS + AUDIO_KBPS) * 1000);
            return Err(format!(
                "{:.0} s doesn't fit in {:.1} MB; trim it to under {most:.0} s",
                plan.duration,
                opts.limit_bytes as f64 / 1e6
            ));
        }
        let job = Job {
            input: &input,
            temp: &temp,
            plan: &plan,
            source: &source,
            audio_stream,
            opts,
            attempt,
        };
        let mut result = Err(String::new());
        if gpu {
            result = job.run(true, progress, cancel);
            if result.as_ref().is_err_and(|e| e != CANCELLED) {
                // Hardware decoders or encoders that don't share textures:
                // copy frames through system memory from now on.
                let _ = std::fs::remove_file(&temp);
                gpu = false;
            }
        }
        if !gpu {
            result = job.run(false, progress, cancel);
        }
        if let Err(e) = result {
            let _ = std::fs::remove_file(&temp);
            return Err(e);
        }
        let bytes = std::fs::metadata(&temp)
            .map_err(|e| format!("{}: {e}", temp.display()))?
            .len();
        if bytes <= opts.limit_bytes {
            // Index first, so players and Discord's preview can start before
            // the whole file has arrived. Kept as written if that fails.
            let done = match faststart(&temp, &fast) {
                Ok(true) => &fast,
                _ => &temp,
            };
            let renamed = std::fs::rename(done, &output);
            let _ = std::fs::remove_file(&temp);
            let _ = std::fs::remove_file(&fast);
            renamed.map_err(|e| format!("{}: {e}", output.display()))?;
            return Ok(Exported {
                path: output,
                bytes,
                width: plan.width,
                height: plan.height,
                fps: plan.fps,
                duration: plan.duration,
                video_kbps: plan.video_kbps,
                audio_kbps: plan.audio_kbps,
                attempts: attempt,
                hardware: gpu,
                from_hdr: source.hdr,
            });
        }
        let _ = std::fs::remove_file(&temp);
        budget *= opts.limit_bytes as f64 / bytes as f64 * RETRY_MARGIN;
    }
    Err(format!(
        "the encoder kept going over {:.1} MB after {MAX_ATTEMPTS} tries",
        opts.limit_bytes as f64 / 1e6
    ))
}

/// `<output>.part` next to the output, so a crash leaves no `.mp4` behind
/// for the library to list.
fn temp_path(output: &Path) -> PathBuf {
    let mut name = output.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    output.with_file_name(name)
}

/// The second temporary file, which `faststart` writes.
fn fast_path(output: &Path) -> PathBuf {
    let mut name = output.file_name().unwrap_or_default().to_os_string();
    name.push(".fast.part");
    output.with_file_name(name)
}

/// Makes the CPU path testable on a machine with a working GPU path.
#[cfg(test)]
fn force_cpu() -> bool {
    std::env::var_os("REC_TEST_EXPORT_CPU").is_some()
}

#[cfg(not(test))]
fn force_cpu() -> bool {
    false
}

/// Copies the MP4 at `src` to `dst` with its `moov` box (the index) moved
/// in front of the first `mdat` and the chunk offsets shifted to match.
/// `Ok(false)` when there's nothing to do or it can't be done (the index is
/// already first, or 32-bit offsets would overflow).
fn faststart(src: &Path, dst: &Path) -> std::io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut file = std::fs::File::open(src)?;
    let len = file.metadata()?.len();
    // Top-level boxes: (start, size, type).
    let mut boxes = Vec::new();
    let mut at = 0u64;
    while at + 8 <= len {
        let mut head = [0u8; 16];
        file.seek(SeekFrom::Start(at))?;
        file.read_exact(&mut head[..8])?;
        let mut size = u64::from(u32::from_be_bytes([head[0], head[1], head[2], head[3]]));
        if size == 1 {
            file.read_exact(&mut head[8..])?;
            size = u64::from_be_bytes([
                head[8], head[9], head[10], head[11], head[12], head[13], head[14], head[15],
            ]);
        } else if size == 0 {
            size = len - at;
        }
        if size < 8 || at + size > len {
            return Ok(false);
        }
        boxes.push((at, size, [head[4], head[5], head[6], head[7]]));
        at += size;
    }
    let find = |kind: &[u8; 4]| boxes.iter().position(|b| &b.2 == kind);
    let (Some(moov), Some(mdat)) = (find(b"moov"), find(b"mdat")) else {
        return Ok(false);
    };
    if moov < mdat {
        return Ok(false);
    }
    let (moov_at, moov_size, _) = boxes[moov];
    let mut index = vec![0u8; usize::try_from(moov_size).map_err(std::io::Error::other)?];
    file.seek(SeekFrom::Start(moov_at))?;
    file.read_exact(&mut index)?;
    if !shift_offsets(&mut index, moov_size) {
        return Ok(false);
    }
    let mut out = std::io::BufWriter::new(std::fs::File::create(dst)?);
    for (i, &(start, size, _)) in boxes.iter().enumerate() {
        if i == mdat {
            out.write_all(&index)?;
        }
        if i != moov {
            file.seek(SeekFrom::Start(start))?;
            std::io::copy(&mut (&mut file).take(size), &mut out)?;
        }
    }
    out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    Ok(true)
}

/// Adds `delta` to every `stco` / `co64` entry inside a `moov` box. False
/// if the boxes don't parse or a 32-bit offset would overflow.
fn shift_offsets(moov: &mut [u8], delta: u64) -> bool {
    fn be32(b: &[u8]) -> u32 {
        u32::from_be_bytes([b[0], b[1], b[2], b[3]])
    }
    fn walk(data: &mut [u8], delta: u64) -> bool {
        let mut at = 0usize;
        while at + 8 <= data.len() {
            let size = be32(&data[at..]) as usize;
            let kind = [data[at + 4], data[at + 5], data[at + 6], data[at + 7]];
            if size < 8 || at + size > data.len() {
                return false;
            }
            let body = &mut data[at + 8..at + size];
            let ok = match &kind {
                b"trak" | b"mdia" | b"minf" | b"stbl" => walk(body, delta),
                b"stco" | b"co64" => shift_table(body, kind == *b"co64", delta),
                _ => true,
            };
            if !ok {
                return false;
            }
            at += size;
        }
        true
    }
    /// The body of an `stco` (32-bit) or `co64` table: version and flags,
    /// a count, then the offsets.
    fn shift_table(body: &mut [u8], wide: bool, delta: u64) -> bool {
        if body.len() < 8 {
            return false;
        }
        let count = be32(&body[4..]) as usize;
        let width = if wide { 8 } else { 4 };
        let Some(table) = body.get_mut(8..8 + count * width) else {
            return false;
        };
        for entry in table.chunks_exact_mut(width) {
            if wide {
                let mut v = [0u8; 8];
                v.copy_from_slice(entry);
                entry.copy_from_slice(&(u64::from_be_bytes(v) + delta).to_be_bytes());
            } else {
                let Ok(v) = u32::try_from(u64::from(be32(entry)) + delta) else {
                    return false;
                };
                entry.copy_from_slice(&v.to_be_bytes());
            }
        }
        true
    }
    moov.len() >= 8 && walk(&mut moov[8..], delta)
}

/// What the clip holds, read without decoding.
#[derive(Debug)]
struct Source {
    duration: f64,
    /// The picture, inside any padding.
    width: u32,
    height: u32,
    fps: u32,
    hdr: bool,
    /// Reader stream indices of the audio tracks, in file order.
    audio: Vec<u32>,
}

impl Source {
    fn probe(path: &Path) -> Result<Self, String> {
        // SAFETY: Media Foundation calls on a reader this function owns.
        unsafe {
            let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None)
                .map_err(|e| format!("can't open {}: {e}", path.display()))?;
            let video = reader
                .GetNativeMediaType(VIDEO, 0)
                .map_err(|_| "the file has no video stream".to_owned())?;
            let (w, h) = size_of_type(&video)?;
            let (w, h) = picture(&video, (w, h));
            let fps = video
                .GetUINT64(&MF_MT_FRAME_RATE)
                .ok()
                .map(|r| ((r >> 32) as f64, f64::from(r as u32)))
                .filter(|&(n, d)| n > 0.0 && d > 0.0)
                .map_or(60, |(n, d)| (n / d).round() as u32);
            let hdr = video
                .GetUINT32(&MF_MT_TRANSFER_FUNCTION)
                .is_ok_and(|t| t == MFVideoTransFunc_2084.0 as u32);
            let mut audio = Vec::new();
            let mut index = 0;
            while let Ok(native) = reader.GetNativeMediaType(index, 0) {
                if native.GetGUID(&MF_MT_MAJOR_TYPE).ok() == Some(MFMediaType_Audio) {
                    audio.push(index);
                }
                index += 1;
            }
            // The MP4 source lists tracks in reverse file order (measured on
            // Win11 26300 with 2 and 3 audio tracks, fragmented or not, and
            // whichever is enabled; its stream identifiers are just 1..n), so
            // the mix, rec's first audio track, is the last audio stream.
            // `exports_under_the_limit` checks this with one tone per track.
            audio.reverse();
            let duration = reader
                .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
                .ok()
                .and_then(|v| u64::try_from(&v).ok())
                .map_or(0.0, |d| d as f64 / HNS);
            if duration <= 0.0 {
                return Err("the clip has no duration".into());
            }
            Ok(Self {
                duration,
                width: w,
                height: h,
                fps,
                hdr,
                audio,
            })
        }
    }
}

/// One encode of the whole range.
struct Job<'a> {
    input: &'a Path,
    temp: &'a Path,
    plan: &'a Plan,
    source: &'a Source,
    audio_stream: Option<u32>,
    opts: &'a ExportOptions,
    attempt: u32,
}

/// A reader set up for a job, its video size and its audio format.
type Opened = (IMFSourceReader, (u32, u32), Option<Pcm>);

/// The output's audio format, as the reader gives it.
#[derive(Clone, Copy, Debug)]
struct Pcm {
    rate: u32,
    channels: u32,
}

impl Pcm {
    fn block(self) -> u32 {
        self.channels * 2
    }
}

impl Job<'_> {
    /// With `gpu`, decoder and encoder share one D3D11 device and frames
    /// stay textures; without, frames are copied to system memory and the
    /// encoder gets no device (decoding still uses the GPU if it can).
    fn run(
        &self,
        gpu: bool,
        progress: &mut dyn FnMut(Progress),
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let device = Gpu::new();
        let (reader, size, pcm) = match (gpu, &device) {
            (true, Ok(device)) => self.reader(Some(device))?,
            (true, Err(e)) => return Err(e.clone()),
            (false, Ok(device)) => self.reader(Some(device)).or_else(|_| self.reader(None))?,
            (false, Err(_)) => self.reader(None)?,
        };
        let shared = if gpu { device.as_ref().ok() } else { None };
        let writer = self.writer(shared, size, pcm)?;
        let mut pass = Pass {
            job: self,
            reader,
            writer,
            size,
            pcm,
            copy: !gpu,
            progress,
            cancel,
            last_progress: Instant::now(),
        };
        pass.run()
    }

    fn reader(&self, device: Option<&Gpu>) -> Result<Opened, String> {
        // SAFETY: Media Foundation calls on interfaces this function owns;
        // every out-pointer is a valid local.
        unsafe {
            let attrs = attributes(4)?;
            if let Some(gpu) = device {
                attrs
                    .SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &gpu.manager)
                    .map_err(ctx("MF_SOURCE_READER_D3D_MANAGER"))?;
                attrs
                    .SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)
                    .map_err(ctx("MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS"))?;
            }
            attrs
                .SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)
                .map_err(ctx("MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING"))?;
            let reader =
                MFCreateSourceReaderFromURL(&HSTRING::from(self.input.as_os_str()), &attrs)
                    .map_err(ctx("MFCreateSourceReaderFromURL"))?;
            reader
                .SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)
                .map_err(ctx("SetStreamSelection"))?;
            reader
                .SetStreamSelection(VIDEO, true)
                .map_err(ctx("SetStreamSelection"))?;

            let wanted = MFCreateMediaType().map_err(ctx("MFCreateMediaType"))?;
            wanted
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(ctx("MF_MT_MAJOR_TYPE"))?;
            wanted
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                .map_err(ctx("MF_MT_SUBTYPE"))?;
            wanted
                .SetUINT64(&MF_MT_FRAME_SIZE, pack(self.plan.width, self.plan.height))
                .map_err(ctx("MF_MT_FRAME_SIZE"))?;
            // SDR BT.709 out, whatever goes in: for HDR10 this asks the
            // video processor to convert PQ BT.2020 down.
            sdr_709(&wanted)?;
            reader
                .SetCurrentMediaType(VIDEO, None, &wanted)
                .map_err(ctx("the video processor can't make NV12"))?;
            let current = reader
                .GetCurrentMediaType(VIDEO)
                .map_err(ctx("GetCurrentMediaType"))?;
            let size = size_of_type(&current)?;
            if size != (self.plan.width, self.plan.height) {
                return Err(format!(
                    "the video processor made {}x{}, not {}x{}",
                    size.0, size.1, self.plan.width, self.plan.height
                ));
            }

            let pcm = match self.audio_stream {
                Some(stream) => Some(audio_output(&reader, stream)?),
                None => None,
            };
            Ok((reader, size, pcm))
        }
    }

    fn writer(
        &self,
        device: Option<&Gpu>,
        (width, height): (u32, u32),
        pcm: Option<Pcm>,
    ) -> Result<IMFSinkWriter, String> {
        let plan = self.plan;
        // SAFETY: Media Foundation calls on interfaces this function owns.
        unsafe {
            let attrs = attributes(4)?;
            attrs
                .SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)
                .map_err(ctx("MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS"))?;
            if let Some(gpu) = device {
                attrs
                    .SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &gpu.manager)
                    .map_err(ctx("MF_SINK_WRITER_D3D_MANAGER"))?;
            }
            // The extension is `.part`, so name the container.
            attrs
                .SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)
                .map_err(ctx("MF_TRANSCODE_CONTAINERTYPE"))?;
            // Not MF_MPEG4SINK_MOOV_BEFORE_MDAT: on Win11 26300 it needs a
            // read/write byte stream and then writes a corrupt file (the
            // mdat header is overwritten). `faststart` moves the index.
            let writer =
                MFCreateSinkWriterFromURL(&HSTRING::from(self.temp.as_os_str()), None, &attrs)
                    .map_err(ctx("MFCreateSinkWriterFromURL"))?;

            let frame_rate = pack(plan.fps, 1);
            let h264 = video_type(&MFVideoFormat_H264, (width, height), frame_rate)?;
            h264.SetUINT32(&MF_MT_AVG_BITRATE, plan.video_kbps * 1000)
                .map_err(ctx("MF_MT_AVG_BITRATE"))?;
            h264.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)
                .map_err(ctx("MF_MT_MPEG2_PROFILE"))?;
            sdr_709(&h264)?;
            let video = writer.AddStream(&h264).map_err(ctx("AddStream (H.264)"))?;
            let nv12 = video_type(&MFVideoFormat_NV12, (width, height), frame_rate)?;
            sdr_709(&nv12)?;
            if device.is_none() {
                nv12.SetUINT32(&MF_MT_DEFAULT_STRIDE, width)
                    .map_err(ctx("MF_MT_DEFAULT_STRIDE"))?;
            }
            // Peak-constrained VBR: the mean decides the size, the peak
            // lets busy scenes borrow from quiet ones.
            let params = attributes(5)?;
            let bps = plan.video_kbps * 1000;
            let _ = params.SetUINT32(
                &CODECAPI_AVEncCommonRateControlMode,
                eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32,
            );
            let _ = params.SetUINT32(&CODECAPI_AVEncCommonMeanBitRate, bps);
            let _ = params.SetUINT32(&CODECAPI_AVEncCommonMaxBitRate, bps / 2 * 3);
            let _ = params.SetUINT32(&CODECAPI_AVEncMPVGOPSize, plan.fps * 2);
            let _ = params.SetUINT32(&CODECAPI_AVEncCommonQualityVsSpeed, 100);
            if writer.SetInputMediaType(video, &nv12, &params).is_err() {
                // An encoder that refuses a parameter: its defaults, at the
                // output type's bitrate.
                writer
                    .SetInputMediaType(video, &nv12, None)
                    .map_err(ctx("the H.264 encoder refused NV12"))?;
            }
            debug_assert_eq!(video, 0);

            if let Some(pcm) = pcm {
                let aac = audio_type(&MFAudioFormat_AAC, pcm)?;
                aac.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, plan.audio_kbps * 125)
                    .map_err(ctx("MF_MT_AUDIO_AVG_BYTES_PER_SECOND"))?;
                aac.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)
                    .map_err(ctx("MF_MT_AAC_PAYLOAD_TYPE"))?;
                let audio = writer.AddStream(&aac).map_err(ctx("AddStream (AAC)"))?;
                let input = audio_type(&MFAudioFormat_PCM, pcm)?;
                input
                    .SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, pcm.block())
                    .map_err(ctx("MF_MT_AUDIO_BLOCK_ALIGNMENT"))?;
                input
                    .SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, pcm.block() * pcm.rate)
                    .map_err(ctx("MF_MT_AUDIO_AVG_BYTES_PER_SECOND"))?;
                input
                    .SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)
                    .map_err(ctx("MF_MT_ALL_SAMPLES_INDEPENDENT"))?;
                writer
                    .SetInputMediaType(audio, &input, None)
                    .map_err(ctx("the AAC encoder refused PCM"))?;
                debug_assert_eq!(audio, 1);
            }
            writer.BeginWriting().map_err(ctx("BeginWriting"))?;
            Ok(writer)
        }
    }
}

/// A D3D11 device behind a DXGI device manager, shared by reader and writer.
struct Gpu {
    _device: ID3D11Device,
    manager: IMFDXGIDeviceManager,
}

impl Gpu {
    fn new() -> Result<Self, String> {
        let device = create_device()?;
        let mut token = 0;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        // SAFETY: valid out-pointers; the device outlives the manager's use
        // of it (both live in `Gpu`).
        unsafe {
            MFCreateDXGIDeviceManager(&mut token, &mut manager)
                .map_err(ctx("MFCreateDXGIDeviceManager"))?;
            let manager = manager.ok_or("MFCreateDXGIDeviceManager returned nothing")?;
            manager
                .ResetDevice(&device, token)
                .map_err(ctx("ResetDevice"))?;
            Ok(Self {
                _device: device,
                manager,
            })
        }
    }
}

/// The read-encode loop of one [`Job`].
struct Pass<'a, 'b> {
    job: &'a Job<'a>,
    reader: IMFSourceReader,
    writer: IMFSinkWriter,
    size: (u32, u32),
    pcm: Option<Pcm>,
    /// Copy frames to system memory before writing them.
    copy: bool,
    progress: &'b mut dyn FnMut(Progress),
    cancel: &'b AtomicBool,
    last_progress: Instant,
}

/// Where a stream is: reading, or finished.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Stream {
    At(i64),
    Done,
}

impl Pass<'_, '_> {
    fn run(&mut self) -> Result<(), String> {
        let job = self.job;
        let plan = job.plan;
        // Clip time 0 is the first video frame (as in `decode`).
        let (first, first_sample) = self.read(VIDEO)?.ok_or("the video has no frames")?;
        let origin = first + (job.opts.start.max(0.0) * HNS).round() as i64;
        let mut pending = Some((first, first_sample));
        if job.opts.start > 0.0 {
            pending = None;
            // Lands on the key frame at or before it (one per second in rec
            // clips); earlier frames are dropped below. Past the end is
            // MF_E_INVALID_POSITION, ruled out in `export`.
            let position = PROPVARIANT::from(origin);
            // SAFETY: GUID_NULL means 100 ns units; `position` outlives it.
            unsafe { self.reader.SetCurrentPosition(&GUID::zeroed(), &position) }
                .map_err(ctx("SetCurrentPosition"))?;
        }
        let length = (plan.duration * HNS).round() as i64;
        let end = origin + length;
        let period = (HNS / f64::from(plan.fps)).round() as i64;
        let decimate = job.source.fps > plan.fps;
        let mut next_slot = 0i64;
        let mut video = Stream::At(origin);
        let mut audio = match job.audio_stream {
            // Selected since the reader was made: a stream selected late
            // loses what the reader has already read past.
            Some(_) => Stream::At(origin),
            None => Stream::Done,
        };
        let mut wrote_video = false;
        while video != Stream::Done || audio != Stream::Done {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(CANCELLED.into());
            }
            let read_video = match (video, audio) {
                (Stream::At(v), Stream::At(a)) => v <= a,
                (Stream::At(_), Stream::Done) => true,
                _ => false,
            };
            if read_video {
                let next = match pending.take() {
                    Some(held) => Some(held),
                    None => self.read(VIDEO)?,
                };
                let Some((at, sample)) = next else {
                    video = Stream::Done;
                    continue;
                };
                video = Stream::At(at);
                if at >= end - EPSILON {
                    video = self.stop(VIDEO)?;
                    continue;
                }
                let t = at - origin;
                if t < -EPSILON {
                    continue;
                }
                let t = t.max(0);
                if decimate {
                    if t < next_slot - period / 4 {
                        continue;
                    }
                    while next_slot <= t + period / 4 {
                        next_slot += period;
                    }
                }
                let sample = if self.copy {
                    self.to_memory(&sample)?
                } else {
                    sample
                };
                // SAFETY: plain setters on a sample this pass owns.
                unsafe {
                    sample.SetSampleTime(t).map_err(ctx("SetSampleTime"))?;
                    sample
                        .SetSampleDuration(period)
                        .map_err(ctx("SetSampleDuration"))?;
                    self.writer
                        .WriteSample(0, &sample)
                        .map_err(ctx("WriteSample (video)"))?;
                }
                wrote_video = true;
                self.report(t as f64 / length.max(1) as f64);
            } else if let Some(stream) = job.audio_stream {
                let Some((at, sample)) = self.read(stream)? else {
                    audio = Stream::Done;
                    continue;
                };
                audio = Stream::At(at);
                if at >= end - EPSILON {
                    audio = self.stop(stream)?;
                    continue;
                }
                if let Some(sample) = self.clip_audio(&sample, at - origin, length)? {
                    // SAFETY: plain call on a valid writer and sample.
                    unsafe { self.writer.WriteSample(1, &sample) }
                        .map_err(ctx("WriteSample (audio)"))?;
                }
            }
        }
        if !wrote_video {
            return Err("no video in the chosen range".into());
        }
        // SAFETY: plain call on a valid writer; writes the index.
        unsafe { self.writer.Finalize() }.map_err(ctx("Finalize"))?;
        (self.progress)(Progress {
            fraction: 1.0,
            pass: job.attempt,
        });
        Ok(())
    }

    /// The next sample of `stream`, or `None` at its end.
    fn read(&mut self, stream: u32) -> Result<Option<(i64, IMFSample)>, String> {
        loop {
            let mut flags = 0u32;
            let mut at = 0i64;
            let mut sample: Option<IMFSample> = None;
            // SAFETY: all out-pointers are valid locals.
            unsafe {
                self.reader.ReadSample(
                    stream,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut at),
                    Some(&mut sample),
                )
            }
            .map_err(ctx("ReadSample"))?;
            let flags = flags as i32;
            if flags & MF_SOURCE_READERF_ERROR.0 != 0 {
                return Err("the decoder failed".into());
            }
            if let Some(sample) = sample {
                return Ok(Some((at, sample)));
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 != 0 {
                return Ok(None);
            }
        }
    }

    /// Stops decoding a stream past the end of the range.
    fn stop(&mut self, stream: u32) -> Result<Stream, String> {
        // SAFETY: plain call on a valid reader.
        unsafe { self.reader.SetStreamSelection(stream, false) }
            .map_err(ctx("SetStreamSelection"))?;
        Ok(Stream::Done)
    }

    /// Audio retimed to the range, cut to it at whole PCM frames: `None`
    /// when the sample lies outside it.
    fn clip_audio(
        &self,
        sample: &IMFSample,
        t: i64,
        length: i64,
    ) -> Result<Option<IMFSample>, String> {
        let pcm = self.pcm.ok_or("audio without a format")?;
        let block = pcm.block() as usize;
        // SAFETY: the buffer is locked while read and unlocked on every
        // path; reads stay inside its current length.
        unsafe {
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(ctx("ConvertToContiguousBuffer"))?;
            let mut data = std::ptr::null_mut();
            let mut len = 0u32;
            buffer
                .Lock(&mut data, None, Some(&mut len))
                .map_err(ctx("Lock"))?;
            let frames = len as usize / block;
            let at = |frame: usize| t + (frame as f64 * HNS / f64::from(pcm.rate)).round() as i64;
            let to_frame = |hns: i64| (hns as f64 * f64::from(pcm.rate) / HNS).round() as i64;
            let first = to_frame(-t).clamp(0, frames as i64) as usize;
            let last = to_frame(length - t).clamp(0, frames as i64) as usize;
            if first >= last {
                let _ = buffer.Unlock();
                return Ok(None);
            }
            if first == 0 && last == frames {
                let _ = buffer.Unlock();
                sample.SetSampleTime(t).map_err(ctx("SetSampleTime"))?;
                return Ok(Some(sample.clone()));
            }
            let bytes = std::slice::from_raw_parts(data, len as usize)[first * block..last * block]
                .to_vec();
            let _ = buffer.Unlock();
            let start = at(first);
            let out = memory_sample(&bytes)?;
            out.SetSampleTime(start).map_err(ctx("SetSampleTime"))?;
            out.SetSampleDuration(at(last) - start)
                .map_err(ctx("SetSampleDuration"))?;
            Ok(Some(out))
        }
    }

    /// An NV12 frame copied out of a (GPU) sample into system memory.
    fn to_memory(&self, sample: &IMFSample) -> Result<IMFSample, String> {
        let (w, h) = self.size;
        // SAFETY: the copy writes exactly the contiguous length the source
        // reports into a buffer of that size; buffers stay referenced for
        // the calls.
        unsafe {
            let buffer = sample
                .GetBufferByIndex(0)
                .map_err(ctx("GetBufferByIndex"))?;
            if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
                let len = two_d
                    .GetContiguousLength()
                    .map_err(ctx("GetContiguousLength"))?;
                let expected = w * h * 3 / 2;
                if len != expected {
                    return Err(format!("NV12 frame of {len} bytes, expected {expected}"));
                }
                let out = MFCreateMemoryBuffer(len).map_err(ctx("MFCreateMemoryBuffer"))?;
                let mut data = std::ptr::null_mut();
                out.Lock(&mut data, None, None).map_err(ctx("Lock"))?;
                let copied =
                    two_d.ContiguousCopyTo(std::slice::from_raw_parts_mut(data, len as usize));
                let _ = out.Unlock();
                copied.map_err(ctx("ContiguousCopyTo"))?;
                out.SetCurrentLength(len).map_err(ctx("SetCurrentLength"))?;
                let copy = MFCreateSample().map_err(ctx("MFCreateSample"))?;
                copy.AddBuffer(&out).map_err(ctx("AddBuffer"))?;
                Ok(copy)
            } else {
                // Already system memory, laid out at the default stride.
                Ok(sample.clone())
            }
        }
    }

    fn report(&mut self, fraction: f64) {
        if self.last_progress.elapsed() >= PROGRESS_EVERY {
            self.last_progress = Instant::now();
            (self.progress)(Progress {
                fraction: fraction.clamp(0.0, 1.0),
                pass: self.job.attempt,
            });
        }
    }
}

/// Asks the reader for 16-bit PCM from audio `stream`, 48 kHz stereo if its
/// resampler can, and returns what it gives.
fn audio_output(reader: &IMFSourceReader, stream: u32) -> Result<Pcm, String> {
    // SAFETY: media types made here and handed to a valid reader.
    unsafe {
        reader
            .SetStreamSelection(stream, true)
            .map_err(ctx("SetStreamSelection (audio)"))?;
        let wanted = audio_type(
            &MFAudioFormat_PCM,
            Pcm {
                rate: RATE,
                channels: CHANNELS,
            },
        )?;
        if reader.SetCurrentMediaType(stream, None, &wanted).is_err() {
            // Any PCM, checked below.
            let any = MFCreateMediaType().map_err(ctx("MFCreateMediaType"))?;
            any.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                .map_err(ctx("MF_MT_MAJOR_TYPE"))?;
            any.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)
                .map_err(ctx("MF_MT_SUBTYPE"))?;
            any.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)
                .map_err(ctx("MF_MT_AUDIO_BITS_PER_SAMPLE"))?;
            reader
                .SetCurrentMediaType(stream, None, &any)
                .map_err(ctx("can't decode the audio track"))?;
        }
        let current = reader
            .GetCurrentMediaType(stream)
            .map_err(ctx("GetCurrentMediaType (audio)"))?;
        let get = |key: &GUID| current.GetUINT32(key).unwrap_or(0);
        let pcm = Pcm {
            rate: get(&MF_MT_AUDIO_SAMPLES_PER_SECOND),
            channels: get(&MF_MT_AUDIO_NUM_CHANNELS),
        };
        let bits = get(&MF_MT_AUDIO_BITS_PER_SAMPLE);
        if !matches!(pcm.rate, 44_100 | 48_000) || !matches!(pcm.channels, 1 | 2) || bits != 16 {
            return Err(format!(
                "audio is {} Hz, {} channels, {bits}-bit; AAC needs 44.1/48 kHz mono or stereo",
                pcm.rate, pcm.channels
            ));
        }
        Ok(pcm)
    }
}

fn attributes(size: u32) -> Result<IMFAttributes, String> {
    let mut attrs: Option<IMFAttributes> = None;
    // SAFETY: valid out-pointer.
    unsafe { MFCreateAttributes(&mut attrs, size) }.map_err(ctx("MFCreateAttributes"))?;
    attrs.ok_or_else(|| "MFCreateAttributes returned nothing".into())
}

fn video_type(subtype: &GUID, size: (u32, u32), frame_rate: u64) -> Result<IMFMediaType, String> {
    // SAFETY: setters on a new media type.
    unsafe {
        let t = MFCreateMediaType().map_err(ctx("MFCreateMediaType"))?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(ctx("MF_MT_MAJOR_TYPE"))?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)
            .map_err(ctx("MF_MT_SUBTYPE"))?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack(size.0, size.1))
            .map_err(ctx("MF_MT_FRAME_SIZE"))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, frame_rate)
            .map_err(ctx("MF_MT_FRAME_RATE"))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))
            .map_err(ctx("MF_MT_PIXEL_ASPECT_RATIO"))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(ctx("MF_MT_INTERLACE_MODE"))?;
        Ok(t)
    }
}

fn audio_type(subtype: &GUID, pcm: Pcm) -> Result<IMFMediaType, String> {
    // SAFETY: setters on a new media type.
    unsafe {
        let t = MFCreateMediaType().map_err(ctx("MFCreateMediaType"))?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
            .map_err(ctx("MF_MT_MAJOR_TYPE"))?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)
            .map_err(ctx("MF_MT_SUBTYPE"))?;
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)
            .map_err(ctx("MF_MT_AUDIO_BITS_PER_SAMPLE"))?;
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, pcm.rate)
            .map_err(ctx("MF_MT_AUDIO_SAMPLES_PER_SECOND"))?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, pcm.channels)
            .map_err(ctx("MF_MT_AUDIO_NUM_CHANNELS"))?;
        if *subtype == MFAudioFormat_PCM {
            t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, pcm.block())
                .map_err(ctx("MF_MT_AUDIO_BLOCK_ALIGNMENT"))?;
            t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, pcm.block() * pcm.rate)
                .map_err(ctx("MF_MT_AUDIO_AVG_BYTES_PER_SECOND"))?;
        }
        Ok(t)
    }
}

/// Tags a video type as limited-range BT.709 SDR.
fn sdr_709(t: &IMFMediaType) -> Result<(), String> {
    // SAFETY: setters on a valid media type.
    unsafe {
        t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)
            .map_err(ctx("MF_MT_TRANSFER_FUNCTION"))?;
        t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)
            .map_err(ctx("MF_MT_VIDEO_PRIMARIES"))?;
        t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)
            .map_err(ctx("MF_MT_YUV_MATRIX"))?;
        t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)
            .map_err(ctx("MF_MT_VIDEO_NOMINAL_RANGE"))?;
    }
    Ok(())
}

/// A sample holding a copy of `bytes` in system memory.
fn memory_sample(bytes: &[u8]) -> Result<IMFSample, String> {
    let len = u32::try_from(bytes.len()).map_err(|_| "sample too large".to_owned())?;
    // SAFETY: the buffer is `len` bytes, locked while `bytes` is copied in.
    unsafe {
        let buffer = MFCreateMemoryBuffer(len).map_err(ctx("MFCreateMemoryBuffer"))?;
        let mut data = std::ptr::null_mut();
        buffer.Lock(&mut data, None, None).map_err(ctx("Lock"))?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
        let _ = buffer.Unlock();
        buffer
            .SetCurrentLength(len)
            .map_err(ctx("SetCurrentLength"))?;
        let sample = MFCreateSample().map_err(ctx("MFCreateSample"))?;
        sample.AddBuffer(&buffer).map_err(ctx("AddBuffer"))?;
        Ok(sample)
    }
}

fn pack(hi: u32, lo: u32) -> u64 {
    u64::from(hi) << 32 | u64::from(lo)
}

fn size_of_type(media: &IMFMediaType) -> Result<(u32, u32), String> {
    // SAFETY: plain query on a valid media type.
    let packed = unsafe { media.GetUINT64(&MF_MT_FRAME_SIZE) }.map_err(ctx("MF_MT_FRAME_SIZE"))?;
    let size = ((packed >> 32) as u32, packed as u32);
    if size.0 == 0 || size.1 == 0 {
        return Err("the video has no frame size".into());
    }
    Ok(size)
}

/// The size of the visible picture (`MF_MT_MINIMUM_DISPLAY_APERTURE`, set
/// when the encoder padded it), or the frame size.
fn picture(media: &IMFMediaType, size: (u32, u32)) -> (u32, u32) {
    let mut area = MFVideoArea::default();
    // SAFETY: the blob is copied into `area`, whose size is passed.
    let read = unsafe {
        media.GetBlob(
            &MF_MT_MINIMUM_DISPLAY_APERTURE,
            std::slice::from_raw_parts_mut((&raw mut area).cast::<u8>(), size_of::<MFVideoArea>()),
            None,
        )
    };
    let (w, h) = (area.Area.cx, area.Area.cy);
    if read.is_ok() && w > 0 && h > 0 && w as u32 <= size.0 && h as u32 <= size.1 {
        (w as u32, h as u32)
    } else {
        size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(limit_mb: u64) -> ExportOptions {
        ExportOptions {
            limit_bytes: limit_mb * 1_000_000,
            ..ExportOptions::default()
        }
    }

    fn bpp(p: &Plan) -> f64 {
        f64::from(p.video_kbps) * 1000.0 / f64::from(p.width * p.height * p.fps)
    }

    #[test]
    fn a_short_clip_in_10_mb_is_720p60() {
        let p = plan(15.7, 1920, 1080, 60, &opts(10));
        assert_eq!((p.width, p.height, p.fps), (1280, 720, 60), "{p:?}");
        assert_eq!(p.audio_kbps, 128);
        assert!(p.fits);
        // The whole payload budget: 10 MB * 0.96 over 15.7 s, less audio.
        let total = 10e6 * PAYLOAD * 8.0 / 15.7 / 1000.0;
        assert_eq!(p.video_kbps, (total - 128.0).floor() as u32);
        assert!(p.estimated_bytes <= 10_000_000);
    }

    #[test]
    fn tight_budgets_drop_to_30_fps_then_smaller_pictures() {
        let p = plan(45.0, 1920, 1080, 60, &opts(10));
        assert_eq!(p.fps, 30, "{p:?}");
        assert!(p.height < 720 && p.height >= 360, "{p:?}");
        assert!(bpp(&p) >= MIN_BPP, "{p:?}");
        assert_eq!(p.audio_kbps, 128);
        let p = plan(120.0, 1920, 1080, 60, &opts(10));
        assert_eq!((p.height, p.fps, p.audio_kbps), (360, 30, 96), "{p:?}");
        assert!(p.fits, "{p:?}");
    }

    #[test]
    fn too_long_doesnt_fit() {
        let p = plan(900.0, 1920, 1080, 60, &opts(10));
        assert!(!p.fits, "{p:?}");
        assert_eq!((p.height, p.fps), (360, 30));
    }

    #[test]
    fn big_limits_keep_the_source_and_cap_the_bitrate() {
        let p = plan(30.0, 2560, 1440, 60, &opts(500));
        assert_eq!((p.width, p.height, p.fps), (2560, 1440, 60), "{p:?}");
        assert!((bpp(&p) - MAX_BPP).abs() < 0.001, "{p:?}");
        assert!(p.estimated_bytes < 500_000_000 / 2, "{p:?}");
    }

    #[test]
    fn never_scales_up_or_speeds_up() {
        let p = plan(10.0, 854, 480, 30, &opts(500));
        assert_eq!((p.width, p.height, p.fps), (854, 480, 30), "{p:?}");
        // Small 60 fps sources keep 60 while their own height has the bits.
        let p = plan(15.0, 854, 480, 60, &opts(10));
        assert_eq!((p.height, p.fps), (480, 60), "{p:?}");
    }

    #[test]
    fn honours_height_and_fps_caps() {
        let o = ExportOptions {
            max_height: Some(720),
            max_fps: Some(30),
            ..opts(50)
        };
        let p = plan(20.0, 1920, 1080, 60, &o);
        assert_eq!((p.width, p.height, p.fps), (1280, 720, 30), "{p:?}");
        // Above 60 is still 60.
        let o = ExportOptions {
            max_fps: Some(144),
            ..opts(50)
        };
        assert_eq!(plan(10.0, 1920, 1080, 144, &o).fps, 60);
    }

    #[test]
    fn the_trim_range_sets_the_duration() {
        let o = ExportOptions {
            start: 50.0,
            end: Some(60.0),
            ..opts(10)
        };
        let p = plan(120.0, 1920, 1080, 60, &o);
        assert!((p.duration - 10.0).abs() < 1e-9);
        let whole = plan(10.0, 1920, 1080, 60, &opts(10));
        assert_eq!(p, whole);
        // An end past the clip is the clip's end.
        let o = ExportOptions {
            start: 5.0,
            end: Some(99.0),
            ..opts(10)
        };
        assert!((plan(20.0, 1920, 1080, 60, &o).duration - 15.0).abs() < 1e-9);
    }

    #[test]
    fn odd_sources_get_even_sizes_in_aspect() {
        let p = plan(10.0, 3440, 1440, 60, &opts(10));
        assert_eq!(p.height % 2, 0);
        assert_eq!(p.width % 2, 0);
        let aspect = f64::from(p.width) / f64::from(p.height);
        assert!((aspect - 3440.0 / 1440.0).abs() < 0.01, "{p:?}");
        let p = plan(10.0, 1081, 607, 60, &opts(500));
        assert_eq!((p.width % 2, p.height % 2), (0, 0), "{p:?}");
    }

    #[test]
    fn estimates_never_pass_the_limit() {
        for &limit in &PRESETS {
            for duration in [1.0, 5.0, 15.0, 30.0, 60.0, 180.0, 600.0] {
                for (w, h, fps) in [(1920, 1080, 60), (2560, 1440, 144), (1280, 720, 30)] {
                    let o = ExportOptions {
                        limit_bytes: limit,
                        ..ExportOptions::default()
                    };
                    let p = plan(duration, w, h, fps, &o);
                    let payload = f64::from(p.video_kbps + p.audio_kbps) * 125.0 * p.duration;
                    assert!(payload <= limit as f64 * PAYLOAD + 1.0, "{p:?}");
                    assert!(p.height <= h && p.fps <= fps.min(60), "{p:?}");
                }
            }
        }
    }

    #[test]
    fn no_audio_gives_video_everything() {
        let o = opts(10);
        let budget = 10e6 * PAYLOAD;
        let p = plan_budget(15.7, (1920, 1080), 60, &o, budget, false);
        assert_eq!(p.audio_kbps, 0);
        assert_eq!(p.video_kbps, (budget * 8.0 / 15.7 / 1000.0).floor() as u32);
    }

    #[test]
    fn temp_files_sit_next_to_the_output() {
        let t = temp_path(Path::new(r"C:\clips\a.mp4"));
        assert_eq!(t, Path::new(r"C:\clips\a.mp4.part"));
    }

    /// A clip to export: `REC_TEST_CLIP`, else a synthetic one made here
    /// (busy 1280x720 30 fps H.264 at 30 Mbps, two AAC tracks).
    fn test_clip(dir: &Path) -> PathBuf {
        if let Some(path) = std::env::var_os("REC_TEST_CLIP") {
            return PathBuf::from(path);
        }
        let path = dir.join("synthetic.mp4");
        synthesize(&path, 8.0, (1280, 720), 30, 2).unwrap();
        path
    }

    /// Writes moving noise and bars with one tone per audio track.
    fn synthesize(
        path: &Path,
        seconds: f64,
        (w, h): (u32, u32),
        fps: u32,
        tracks: u32,
    ) -> Result<(), String> {
        let pcm = Pcm {
            rate: RATE,
            channels: CHANNELS,
        };
        // SAFETY: Media Foundation calls on interfaces this test owns.
        unsafe {
            let attrs = attributes(2)?;
            attrs
                .SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)
                .map_err(ctx("hw"))?;
            let writer = MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, &attrs)
                .map_err(ctx("MFCreateSinkWriterFromURL"))?;
            let rate = pack(fps, 1);
            let out = video_type(&MFVideoFormat_H264, (w, h), rate)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, 30_000_000)
                .map_err(ctx("bitrate"))?;
            writer.AddStream(&out).map_err(ctx("AddStream"))?;
            let input = video_type(&MFVideoFormat_NV12, (w, h), rate)?;
            input
                .SetUINT32(&MF_MT_DEFAULT_STRIDE, w)
                .map_err(ctx("stride"))?;
            writer
                .SetInputMediaType(0, &input, None)
                .map_err(ctx("SetInputMediaType"))?;
            for _ in 0..tracks {
                let aac = audio_type(&MFAudioFormat_AAC, pcm)?;
                aac.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 24_000)
                    .map_err(ctx("aac"))?;
                let stream = writer.AddStream(&aac).map_err(ctx("AddStream"))?;
                writer
                    .SetInputMediaType(stream, &audio_type(&MFAudioFormat_PCM, pcm)?, None)
                    .map_err(ctx("SetInputMediaType"))?;
            }
            writer.BeginWriting().map_err(ctx("BeginWriting"))?;
            let frames = (seconds * f64::from(fps)) as u32;
            let period = (HNS / f64::from(fps)) as i64;
            let mut seed = 0x2545_f491_u32;
            let mut frame = vec![0u8; (w * h * 3 / 2) as usize];
            let chunk = RATE / fps;
            for n in 0..frames {
                for (i, px) in frame[..(w * h) as usize].iter_mut().enumerate() {
                    let x = i as u32 % w;
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    let bar = ((x + n * 8) / 64 % 2) as u8 * 120 + 40;
                    *px = bar.wrapping_add(seed as u8 >> 3);
                }
                for (i, c) in frame[(w * h) as usize..].iter_mut().enumerate() {
                    *c = (128 + (i as u32 / 2 + n) % 64) as u8;
                }
                let sample = memory_sample(&frame)?;
                sample
                    .SetSampleTime(i64::from(n) * period)
                    .map_err(ctx("time"))?;
                sample.SetSampleDuration(period).map_err(ctx("dur"))?;
                writer.WriteSample(0, &sample).map_err(ctx("WriteSample"))?;
                for track in 0..tracks {
                    let hz = 440.0 * f64::from(track + 1);
                    let mut bytes = Vec::with_capacity((chunk * 4) as usize);
                    for k in 0..chunk {
                        let t = f64::from(n * chunk + k) / f64::from(RATE);
                        let v = ((t * hz * std::f64::consts::TAU).sin() * 8000.0) as i16;
                        bytes.extend_from_slice(&v.to_le_bytes());
                        bytes.extend_from_slice(&v.to_le_bytes());
                    }
                    let sample = memory_sample(&bytes)?;
                    sample
                        .SetSampleTime(i64::from(n) * period)
                        .map_err(ctx("time"))?;
                    sample.SetSampleDuration(period).map_err(ctx("dur"))?;
                    writer
                        .WriteSample(track + 1, &sample)
                        .map_err(ctx("WriteSample"))?;
                }
            }
            writer.Finalize().map_err(ctx("Finalize"))?;
        }
        Ok(())
    }

    /// The first top-level MP4 box types.
    fn boxes(path: &Path) -> Vec<String> {
        let data = std::fs::read(path).unwrap();
        let mut at = 0usize;
        let mut out = Vec::new();
        while at + 16 <= data.len() && out.len() < 6 {
            let size = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
            out.push(String::from_utf8_lossy(&data[at + 4..at + 8]).into_owned());
            let size = if size == 1 {
                u64::from_be_bytes(data[at + 8..at + 16].try_into().unwrap()) as usize
            } else {
                size
            };
            if size < 8 {
                break;
            }
            at += size;
        }
        out
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rec-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Exports a clip and checks the result: `REC_TEST_CLIP=<clip.mp4>
    /// cargo test --release -p rec-library export -- --ignored --nocapture`
    /// (a synthetic clip without it). `REC_TEST_LIMIT_MB` sets the limit.
    #[test]
    #[ignore = "needs Media Foundation encoders and decoders (a GPU for the fast path)"]
    fn exports_under_the_limit() {
        crate::sys::init().unwrap();
        let dir = scratch();
        let input = test_clip(&dir);
        let in_bytes = std::fs::metadata(&input).unwrap().len();
        let limit = std::env::var("REC_TEST_LIMIT_MB")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .map_or(DEFAULT_LIMIT.min(in_bytes / 3), |mb| (mb * 1e6) as u64);
        let output = dir.join("out.mp4");
        let o = ExportOptions {
            limit_bytes: limit,
            ..ExportOptions::default()
        };
        let mut calls = 0;
        let started = Instant::now();
        let done = export(
            &input,
            &output,
            &o,
            &mut |p| {
                calls += 1;
                assert!((0.0..=1.0).contains(&p.fraction));
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        let took = started.elapsed().as_secs_f64();
        eprintln!(
            "{} ({in_bytes} bytes) -> {done:?}\n  {took:.2} s for {:.1} s of video \
             ({:.1}x realtime), {:.1}% of the limit, {calls} progress calls, boxes {:?}",
            input.display(),
            done.duration,
            done.duration / took,
            done.bytes as f64 * 100.0 / limit as f64,
            boxes(&output)
        );
        assert!(done.bytes <= limit);
        assert_eq!(done.bytes, std::fs::metadata(&output).unwrap().len());
        assert!(!temp_path(&output).exists() && !fast_path(&output).exists());
        let top = boxes(&output);
        let at = |kind: &str| top.iter().position(|b| b == kind);
        assert!(at("moov").unwrap() < at("mdat").unwrap(), "{top:?}");
        let mut d = crate::decode::Decoder::open(&output, None).unwrap();
        assert!((d.duration - done.duration).abs() < 0.25, "{}", d.duration);
        let mid = d.frame_at(d.duration / 2.0).unwrap();
        assert_eq!((mid.width, mid.height), (done.width, done.height));
        assert!(calls >= 1);

        // A trimmed range, with the last audio track.
        let tracks = Source::probe(&input).unwrap().audio.len();
        let o = ExportOptions {
            start: 1.5,
            end: Some(4.0),
            audio_track: tracks.saturating_sub(1),
            ..o
        };
        let trimmed = dir.join("trimmed.mp4");
        let done = export(&input, &trimmed, &o, &mut |_| {}, &AtomicBool::new(false)).unwrap();
        eprintln!("trimmed -> {done:?}");
        let d = crate::decode::Decoder::open(&trimmed, None).unwrap();
        assert!((d.duration - 2.5).abs() < 0.15, "{}", d.duration);

        // Cancelling leaves nothing behind.
        let cancel = AtomicBool::new(false);
        let cancelled = dir.join("cancelled.mp4");
        let err = export(
            &input,
            &cancelled,
            &ExportOptions::default(),
            &mut |_| cancel.store(true, Ordering::Relaxed),
            &cancel,
        )
        .unwrap_err();
        assert_eq!(err, CANCELLED);
        assert!(!cancelled.exists() && !temp_path(&cancelled).exists());
        if std::env::var_os("REC_TEST_KEEP").is_none() {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// The frequency of the tone in a file's first audio track, from the
    /// zero crossings of its left channel.
    fn tone_hz(path: &Path) -> f64 {
        // SAFETY: Media Foundation calls on a reader this test owns; the
        // buffer is read only while locked.
        unsafe {
            let reader =
                MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None).unwrap();
            let stream = Source::probe(path).unwrap().audio[0];
            let pcm = audio_output(&reader, stream).unwrap();
            reader
                .SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)
                .unwrap();
            reader.SetStreamSelection(stream, true).unwrap();
            let (mut crossings, mut frames, mut last) = (0u64, 0u64, 0i16);
            loop {
                let mut flags = 0u32;
                let mut sample = None;
                reader
                    .ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))
                    .unwrap();
                let Some(sample) = sample else {
                    if flags as i32 & MF_SOURCE_READERF_ENDOFSTREAM.0 != 0 {
                        break;
                    }
                    continue;
                };
                let buffer = sample.ConvertToContiguousBuffer().unwrap();
                let mut data = std::ptr::null_mut();
                let mut len = 0u32;
                buffer.Lock(&mut data, None, Some(&mut len)).unwrap();
                let bytes = std::slice::from_raw_parts(data, len as usize);
                for frame in bytes.chunks_exact(pcm.block() as usize) {
                    let v = i16::from_le_bytes([frame[0], frame[1]]);
                    if (v >= 0) != (last >= 0) {
                        crossings += 1;
                    }
                    last = v;
                    frames += 1;
                }
                buffer.Unlock().unwrap();
            }
            crossings as f64 / 2.0 / (frames as f64 / f64::from(pcm.rate))
        }
    }

    /// Track n of the export is track n of the clip (0, the mix, first):
    /// `cargo test --release -p rec-library audio_tracks -- --ignored`.
    #[test]
    #[ignore = "needs Media Foundation encoders and decoders"]
    fn audio_tracks_keep_their_order() {
        crate::sys::init().unwrap();
        let dir = scratch().join("tracks");
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("three.mp4");
        synthesize(&input, 3.0, (640, 360), 30, 3).unwrap();
        for track in 0..3 {
            let output = dir.join(format!("track{track}.mp4"));
            let o = ExportOptions {
                end: Some(2.0),
                audio_track: track,
                ..ExportOptions::default()
            };
            export(&input, &output, &o, &mut |_| {}, &AtomicBool::new(false)).unwrap();
            let hz = tone_hz(&output);
            let want = 440.0 * (track + 1) as f64;
            assert!((hz - want).abs() < want * 0.03, "track {track}: {hz:.0} Hz");
        }
        let err = export(
            &input,
            &dir.join("track3.mp4"),
            &ExportOptions {
                audio_track: 3,
                ..ExportOptions::default()
            },
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(err.contains("3 audio tracks"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
