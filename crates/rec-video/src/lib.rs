//! rec's video pipeline: Windows.Graphics.Capture (or, where WGC cannot hide
//! its yellow border, Desktop Duplication; see `api.rs`) → D3D11 colour conversion
//! (scale, letterbox; BGRA → NV12, or for HDR displays FP16 → P010 HDR10 or
//! tone-mapped NV12) → hardware encoder (AMD AMF or Media Foundation) →
//! H.264 / HEVC / AV1 [`Packet`]s that follow the contract in
//! [`rec_core::media`].
//!
//! Frames never leave the GPU. Per captured frame the GPU does one
//! `CopyResource`; per encode tick it does one conversion (skipped when the
//! previous picture can simply be resubmitted) and the hardware encode. No thread spins: the encode thread sleeps on a high-resolution
//! waitable timer and the capture callback runs on the system thread pool.
//!
//! The caller must have called `MFStartup` once for the process. The
//! pipeline's own threads initialise COM (MTA) themselves.
//!
//! ```text
//!  WGC FrameArrived (thread pool)            encode thread (above normal)
//!  ────────────────────────────              ───────────────────────────────
//!  CopyResource(frame → latest)  ──latest──► every 1/fps: poll encoder events
//!  (pool recreated on resize)                 VideoProcessorBlt(latest → NV12 slot)
//!                                             ProcessInput(slot) ─► HW MFT
//!                                             METransformHaveOutput ─► sink(Packet)
//! ```
//!
//! # Platform notes (measured on Windows 11 build 26200, AMD RX 7800 XT)
//!
//! * **Yellow border:** `IsBorderRequired = false` works for an unpackaged
//!   process without asking; `GraphicsCaptureAccess::RequestAccessAsync(
//!   Borderless)` returns `Allowed` immediately with no prompt. The request is
//!   only made if setting the property fails (packaged builds).
//! * **`MinUpdateInterval`** is set to half the tick period (see
//!   `capture::min_update_interval`): with a full period, WGC delivers the
//!   first game frame *after* the interval, so a 144 Hz game would arrive at
//!   48 fps and a 60 fps encode would repeat frames.
//! * **`GraphicsCaptureItem.Closed`** was not raised when a captured window
//!   was destroyed, so the encode thread also polls `IsWindow` /
//!   `GetMonitorInfoW` once per second.
//! * **AMD MF encoders** (`AMDh264Encoder`, `AMDh265Encoder`) accept and
//!   report peak-constrained VBR; setting the media types resets the codec
//!   properties, so they are applied again afterwards. Every IDR arrives as
//!   AUD + (VPS) + SPS + PPS + IDR, with no B-frames.

pub mod annexb;
mod api;
pub mod geometry;
pub mod obu;
mod priority;
mod probe;
pub mod sei;

#[cfg(windows)]
mod amf;
#[cfg(windows)]
mod backend;
#[cfg(windows)]
mod capture;
#[cfg(windows)]
mod convert;
#[cfg(windows)]
mod d3d;
#[cfg(windows)]
mod dda;
#[cfg(windows)]
mod display;
#[cfg(windows)]
mod encoder;
#[cfg(windows)]
mod pipeline;
#[cfg(windows)]
mod pool;
#[cfg(windows)]
mod shader;
#[cfg(windows)]
mod util;

pub use api::CaptureApi;
#[cfg(windows)]
pub use display::{DisplayColor, DisplayState, display_for};

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub use rec_core::media::{VideoCodec, VideoFormat};
pub use rec_core::replay::{Packet, TrackId};
use rec_core::time::Hns;

/// What to capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureSource {
    /// A top-level window (`HWND` as an integer).
    Window(isize),
    /// A monitor (`HMONITOR` as an integer).
    Monitor(isize),
    /// The primary monitor.
    PrimaryMonitor,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CodecPreference {
    /// HEVC if the adapter has a hardware HEVC encoder, else H.264.
    #[default]
    Auto,
    H264,
    Hevc,
    Av1,
}

/// What to do when the captured display is in HDR mode. On an SDR display
/// all three record SDR the same way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HdrMode {
    /// Record HDR10 (10-bit HEVC or AV1, BT.2020 PQ, static metadata);
    /// tone-map to SDR when the codec or encoder cannot do 10-bit.
    #[default]
    Auto,
    /// Tone-map the HDR picture to SDR: plays right everywhere.
    Tonemap,
    /// Capture 8-bit SDR as Windows presents it; HDR highlights clip.
    Off,
}

/// Which hardware encoder implementation to use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EncoderBackend {
    /// The GPU vendor's native encoder where rec has one (AMD AMF), else
    /// Media Foundation.
    #[default]
    Auto,
    /// AMD AMF, falling back to Media Foundation if it cannot start.
    Amf,
    /// The Media Foundation hardware encoder only.
    MediaFoundation,
}

/// How rec's own GPU work (the capture copy and colour conversion) is
/// scheduled against other apps' (the game's); see decision D15. Only
/// rec's device changes: the game is never touched.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GpuPriority {
    /// Normal, raised to high while a saturated GPU makes frames drop, and
    /// lowered again (with hysteresis) once they do not, or when raising it
    /// does not help.
    #[default]
    Adaptive,
    /// Always normal, like any other app.
    Normal,
    /// Always high.
    High,
}

/// A GPU priority change (or a note about it) made by the encode thread;
/// see [`VideoCapture::take_gpu_priority_changes`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuPriorityChange {
    /// QPC time.
    pub time: Hns,
    /// `IDXGIDevice::SetGPUThreadPriority` level now in force: 0 normal,
    /// 7 high.
    pub level: i32,
    /// Why, for the log.
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoCaptureConfig {
    pub source: CaptureSource,
    /// Which OS capture API to use; see [`CaptureApi`].
    pub capture: CaptureApi,
    /// Encode tick rate (constant frame rate), 1..=240.
    pub fps: u32,
    /// Downscale so the encoded height is at most this (aspect ratio kept).
    pub max_height: Option<u32>,
    pub codec: CodecPreference,
    pub encoder: EncoderBackend,
    pub hdr: HdrMode,
    /// Treat the display as if it were in HDR mode: exercises the HDR path
    /// (FP16 capture, HDR10 or tone-mapped output) on an SDR display. For
    /// testing.
    pub assume_hdr_display: bool,
    /// Mean bitrate in kbit/s.
    pub bitrate_kbps: u32,
    /// Draw the mouse cursor into the capture (WGC only: Desktop
    /// Duplication never draws it).
    pub cursor: bool,
    pub gpu_priority: GpuPriority,
    pub track: TrackId,
    pub segment: u32,
}

impl VideoCaptureConfig {
    /// Defaults: 60 fps, native size, capture API, codec, encoder and HDR auto,
    /// 20 Mbit/s, cursor on, adaptive GPU priority, video track 0,
    /// segment 0.
    pub fn new(source: CaptureSource) -> Self {
        Self {
            source,
            capture: CaptureApi::Auto,
            fps: 60,
            max_height: None,
            codec: CodecPreference::Auto,
            encoder: EncoderBackend::Auto,
            hdr: HdrMode::Auto,
            assume_hdr_display: false,
            bitrate_kbps: 20_000,
            cursor: true,
            gpu_priority: GpuPriority::Adaptive,
            track: rec_core::media::VIDEO_TRACK,
            segment: 0,
        }
    }

    fn validate(&self) -> Result<(), VideoError> {
        if !(1..=240).contains(&self.fps) {
            return Err(VideoError::InvalidConfig(format!(
                "fps {} is outside 1..=240",
                self.fps
            )));
        }
        validate_bitrate(self.bitrate_kbps)?;
        if self.max_height.is_some_and(|h| h < 16) {
            return Err(VideoError::InvalidConfig(
                "max_height must be at least 16".into(),
            ));
        }
        Ok(())
    }
}

fn validate_bitrate(kbps: u32) -> Result<(), VideoError> {
    if kbps == 0 || kbps > 1_000_000 {
        return Err(VideoError::InvalidConfig(format!(
            "bitrate {kbps} kbit/s is outside 1..=1000000"
        )));
    }
    Ok(())
}

/// Counters for monitoring the pipeline. Cheap to take at any time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VideoStats {
    /// Frames delivered by Windows.Graphics.Capture and copied.
    pub frames_captured: u64,
    /// Encode ticks submitted to the encoder (new and repeated frames).
    pub frames_submitted: u64,
    /// Of `frames_submitted`, ticks that repeated the previous picture
    /// because no new frame had arrived.
    pub frames_repeated: u64,
    /// Encoded packets handed to the sink.
    pub frames_encoded: u64,
    /// Encode ticks with no frame submitted: the encoder could not accept
    /// input, no NV12 surface was free, or the encode thread woke up late.
    pub frames_dropped: u64,
    /// Converted frames whose GPU work was still unfinished three frame
    /// periods after it was queued: the GPU is too busy (a game saturating
    /// it) to run rec's work on time, and frames start to drop. Such frames
    /// are still encoded, late.
    pub frames_late: u64,
    /// The GPU priority level in force (0 normal, 7 high).
    pub gpu_priority: i32,
    /// Keyframes that lacked parameter sets and had cached ones inserted.
    pub parameter_sets_inserted: u64,
    /// QPC time the newest frame was captured; `None` before the first.
    /// Windows only delivers a frame when the source changed, but a game
    /// changes it with every present: a game in the foreground whose frames
    /// stop arriving is not being captured (true exclusive fullscreen).
    pub last_frame_time: Option<Hns>,
    /// While the encoded picture is black: since when (QPC time of the first
    /// black picture checked). Checked every two seconds.
    pub black_since: Option<Hns>,
    /// Size of the newest captured frame (with Desktop Duplication, of the
    /// window's part of its display). It follows the source (a window
    /// resized, a game switching resolution) while the encoded size stays
    /// fixed for the pipeline's lifetime: the picture is scaled to fit, and
    /// the caller decides when a restart at the new size is worth it.
    pub source_size: Option<(u32, u32)>,
    /// The captured window or monitor went away; the pipeline has stopped.
    pub source_closed: bool,
    /// The encode thread is running.
    pub running: bool,
    /// The error that stopped the pipeline (or a non-fatal anomaly).
    pub last_error: Option<String>,
}

/// Facts about the running pipeline, as they were when it started. Almost
/// everything is fixed for the session; the exception is the bit rate, which
/// [`VideoCapture::set_bitrate`] can change: the mean/max in
/// [`rate_control`](Self::rate_control) then describe the start state, and
/// the current mean is what `set_bitrate` last returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoInfo {
    /// Codec, size, rate and colour (SDR, or HDR10 with its metadata).
    pub format: VideoFormat,
    /// The capture API in use: [`CaptureApi::Wgc`] or [`CaptureApi::Dda`].
    pub capture: CaptureApi,
    /// Which encoder runs: the native backend or the Media Foundation MFT's
    /// friendly name.
    pub encoder_name: String,
    /// DXGI description of the adapter the capture/encode device lives on.
    pub adapter_name: String,
    /// The adapter is the one whose output shows the captured monitor.
    pub adapter_owns_monitor: bool,
    /// The adapter's LUID; compare with [`adapter_for`] to notice that the
    /// source moved to another GPU.
    pub adapter_luid: i64,
    /// Rate-control mode (with the mean/max bit rate) the encoder reported
    /// after configuration; not updated by `set_bitrate`.
    pub rate_control: String,
    /// The visible picture inside the encoded frame: `format`'s size unless
    /// the encoder needed padding for alignment (the picture is centred).
    /// This is the size the source was scaled to.
    pub picture_width: u32,
    pub picture_height: u32,
    /// Source size when the session started.
    pub source_width: u32,
    pub source_height: u32,
    /// Whether Windows draws the yellow capture border: `Some(false)` when it
    /// was turned off, `None` when the OS has no `IsBorderRequired` property
    /// or Desktop Duplication captures (it has no border).
    pub border_required: Option<bool>,
    /// `MinUpdateInterval` applied to the capture session, if supported; with
    /// Desktop Duplication, the least time between two acquired frames.
    pub min_update_interval_hns: Option<i64>,
    /// The source's display was in HDR mode (or assumed to be) at start;
    /// compare with [`display_for`] to notice HDR being switched.
    pub display_hdr: bool,
    /// Fallbacks taken while starting, e.g. a native encoder that could not
    /// start or HDR10 that had to be tone-mapped. Worth logging.
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VideoError {
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    #[error("Windows.Graphics.Capture is not supported on this system")]
    CaptureUnsupported,
    #[error("no hardware {codec} encoder on adapter \"{adapter}\"")]
    NoEncoder {
        codec: &'static str,
        adapter: String,
    },
    #[error("{context}: {message} (HRESULT 0x{code:08X})")]
    Windows {
        context: &'static str,
        code: u32,
        message: String,
    },
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Default)]
pub(crate) struct SharedStats {
    pub captured: AtomicU64,
    pub submitted: AtomicU64,
    pub repeated: AtomicU64,
    pub encoded: AtomicU64,
    pub dropped: AtomicU64,
    pub late: AtomicU64,
    pub gpu_priority: AtomicI32,
    /// Changes not yet taken by the caller, oldest first (a few at most).
    pub priority_changes: Mutex<Vec<GpuPriorityChange>>,
    pub ps_inserted: AtomicU64,
    /// QPC time of the newest captured frame; 0 = none yet.
    pub last_frame: AtomicI64,
    /// 0 = not black (or not checked yet).
    pub black_since: AtomicI64,
    /// `width << 32 | height` of the newest captured frame; 0 = none yet.
    pub source_size: AtomicU64,
    pub source_closed: AtomicBool,
    pub running: AtomicBool,
    /// Set by any thread on a fatal error; the encode thread then stops.
    pub failed: AtomicBool,
    pub last_error: Mutex<Option<String>>,
}

impl SharedStats {
    pub(crate) fn set_error(&self, err: impl ToString) {
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = Some(err.to_string());
        }
    }

    /// Queues a priority change for the caller; keeps the newest 16 if
    /// nobody takes them.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn push_priority(&self, change: GpuPriorityChange) {
        if let Ok(mut changes) = self.priority_changes.lock() {
            if changes.len() >= 16 {
                changes.remove(0);
            }
            changes.push(change);
        }
    }

    pub(crate) fn fail(&self, err: impl ToString) {
        self.set_error(err);
        self.failed.store(true, Ordering::Release);
    }

    fn snapshot(&self) -> VideoStats {
        VideoStats {
            frames_captured: self.captured.load(Ordering::Relaxed),
            frames_submitted: self.submitted.load(Ordering::Relaxed),
            frames_repeated: self.repeated.load(Ordering::Relaxed),
            frames_encoded: self.encoded.load(Ordering::Relaxed),
            frames_dropped: self.dropped.load(Ordering::Relaxed),
            frames_late: self.late.load(Ordering::Relaxed),
            gpu_priority: self.gpu_priority.load(Ordering::Relaxed),
            parameter_sets_inserted: self.ps_inserted.load(Ordering::Relaxed),
            last_frame_time: Some(self.last_frame.load(Ordering::Relaxed)).filter(|&t| t != 0),
            black_since: Some(self.black_since.load(Ordering::Relaxed)).filter(|&t| t != 0),
            source_size: Some(self.source_size.load(Ordering::Relaxed))
                .filter(|&s| s != 0)
                .map(|s| ((s >> 32) as u32, s as u32)),
            source_closed: self.source_closed.load(Ordering::Acquire),
            running: self.running.load(Ordering::Acquire),
            last_error: self.last_error.lock().ok().and_then(|e| e.clone()),
        }
    }
}

/// Packet consumer; called on the encode thread for every encoded frame.
pub type PacketSink = Box<dyn FnMut(Packet) + Send>;

/// How long [`VideoCapture::stop`] waits for the encode thread before
/// detaching it (only a hung driver call could take this long).
pub const STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How long [`VideoCapture::set_bitrate`] waits for the encode thread.
#[cfg(windows)]
const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// A running capture → encode pipeline. Dropping it stops and joins it.
#[derive(Debug)]
pub struct VideoCapture {
    info: VideoInfo,
    stats: Arc<SharedStats>,
    #[cfg(windows)]
    stop: Arc<util::Event>,
    /// Signalled (or disconnected) when the encode thread has finished.
    done: Option<std::sync::mpsc::Receiver<()>>,
    /// Requests to the encode thread; see [`VideoCapture::set_bitrate`].
    #[cfg(windows)]
    commands: std::sync::mpsc::Sender<pipeline::Command>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl VideoCapture {
    /// Builds the pipeline on a new encode thread and starts capturing.
    /// Returns once capture is running, or the setup error.
    pub fn start(config: VideoCaptureConfig, sink: PacketSink) -> Result<Self, VideoError> {
        config.validate()?;
        Self::start_impl(config, sink)
    }

    #[cfg(windows)]
    fn start_impl(config: VideoCaptureConfig, sink: PacketSink) -> Result<Self, VideoError> {
        let stats = Arc::new(SharedStats::default());
        let stop = Arc::new(util::Event::new()?);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let (command_tx, command_rx) = std::sync::mpsc::channel();
        let thread = {
            let stats = stats.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("rec-video-encode".into())
                .spawn(move || {
                    pipeline::run(config, sink, stats, stop, ready_tx, done_tx, command_rx)
                })
                .map_err(|e| VideoError::Other(format!("cannot spawn encode thread: {e}")))?
        };
        match ready_rx.recv() {
            Ok(Ok(info)) => Ok(Self {
                info,
                stats,
                stop,
                done: Some(done_rx),
                commands: command_tx,
                thread: Some(thread),
            }),
            Ok(Err(err)) => {
                let _ = thread.join();
                Err(err)
            }
            Err(_) => {
                let _ = thread.join();
                Err(VideoError::Other(
                    "encode thread exited during setup".into(),
                ))
            }
        }
    }

    #[cfg(not(windows))]
    fn start_impl(_config: VideoCaptureConfig, _sink: PacketSink) -> Result<Self, VideoError> {
        Err(VideoError::CaptureUnsupported)
    }

    pub fn format(&self) -> &VideoFormat {
        &self.info.format
    }

    pub fn encoder_name(&self) -> &str {
        &self.info.encoder_name
    }

    pub fn adapter_name(&self) -> &str {
        &self.info.adapter_name
    }

    pub fn info(&self) -> &VideoInfo {
        &self.info
    }

    pub fn stats(&self) -> VideoStats {
        self.stats.snapshot()
    }

    /// GPU priority changes since the last call, oldest first: worth
    /// logging. Empty unless the priority is adaptive and the GPU got
    /// saturated, or it is fixed at high.
    pub fn take_gpu_priority_changes(&self) -> Vec<GpuPriorityChange> {
        self.stats
            .priority_changes
            .lock()
            .map(|mut c| std::mem::take(&mut *c))
            .unwrap_or_default()
    }

    /// Changes the encoder's mean bit rate to `kbps` kbit/s (peak 1.5 x mean)
    /// while the pipeline keeps running: no restart, so the segment and the
    /// packet stream (timestamps, IDR every second) continue unbroken and a
    /// replay buffer keeps its history. Returns the mean bit rate in kbit/s
    /// the encoder reports afterwards (`kbps` if it does not report one).
    ///
    /// Synchronous: the encode thread applies the change at its next tick,
    /// so normally this takes at most about one frame period. It fails if
    /// `kbps` is outside 1..=1000000, the encoder refuses the change, the
    /// pipeline is not running, or the encode thread does not answer within
    /// one second (the change may still be applied late). Treat any error as
    /// "restart the pipeline instead".
    ///
    /// [`VideoInfo::rate_control`] keeps describing the start state.
    pub fn set_bitrate(&self, kbps: u32) -> Result<u32, VideoError> {
        validate_bitrate(kbps)?;
        self.set_bitrate_impl(kbps)
    }

    #[cfg(windows)]
    fn set_bitrate_impl(&self, kbps: u32) -> Result<u32, VideoError> {
        use std::sync::mpsc::RecvTimeoutError;
        const NOT_RUNNING: &str = "video pipeline is not running";
        if !self.stats.running.load(Ordering::Acquire) {
            return Err(VideoError::Other(NOT_RUNNING.into()));
        }
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        self.commands
            .send(pipeline::Command::SetBitrate { kbps, reply })
            .map_err(|_| VideoError::Other(NOT_RUNNING.into()))?;
        match answer.recv_timeout(COMMAND_TIMEOUT) {
            Ok(result) => result,
            // The thread dropped the request unanswered: it is exiting.
            Err(RecvTimeoutError::Disconnected) => Err(VideoError::Other(NOT_RUNNING.into())),
            Err(RecvTimeoutError::Timeout) => Err(VideoError::Other(
                "encode thread did not answer the bit rate change in time".into(),
            )),
        }
    }

    #[cfg(not(windows))]
    fn set_bitrate_impl(&self, _kbps: u32) -> Result<u32, VideoError> {
        Err(VideoError::CaptureUnsupported)
    }

    /// Stops capture and encoding and joins the encode thread. Packets still
    /// inside the encoder are discarded.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        #[cfg(windows)]
        self.stop.set();
        let Some(thread) = self.thread.take() else {
            return;
        };
        let finished = match self.done.take() {
            Some(done) => !matches!(
                done.recv_timeout(STOP_TIMEOUT),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ),
            None => true,
        };
        if finished {
            let _ = thread.join();
        } else {
            // Never hang the caller: leave the stuck thread detached.
            self.stats.set_error(format!(
                "encode thread did not stop within {} s and was detached",
                STOP_TIMEOUT.as_secs()
            ));
        }
    }
}

impl Drop for VideoCapture {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// LUID of the adapter a pipeline for `source` would run on now: the one
/// driving the source's monitor. Differs from [`VideoInfo::adapter_luid`]
/// once the window moved to a monitor on another GPU, or a hybrid laptop
/// switched its panel to the other GPU.
#[cfg(windows)]
pub fn adapter_for(source: CaptureSource) -> Result<i64, VideoError> {
    d3d::adapter_luid(source)
}

/// Outcome of [`request_borderless_access`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BorderlessAccess {
    Allowed,
    DeniedByUser,
    DeniedBySystem,
    NotDeclaredByApp,
    UserPromptRequired,
    Unknown,
    /// The OS has no `GraphicsCaptureAccess` API (before Windows 11).
    Unsupported,
}

/// Asks Windows for permission to capture without the yellow border
/// (`GraphicsCaptureAccess::RequestAccessAsync(Borderless)`). Blocks until
/// the answer arrives, so call it from a worker thread.
#[cfg(windows)]
pub fn request_borderless_access() -> Result<BorderlessAccess, VideoError> {
    capture::request_borderless_access()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_can_move_between_threads() {
        fn assert_send<T: Send>() {}
        assert_send::<VideoCapture>();
        assert_send::<VideoStats>();
    }

    #[test]
    fn config_validation() {
        let ok = VideoCaptureConfig::new(CaptureSource::PrimaryMonitor);
        assert!(ok.validate().is_ok());
        for bad in [
            VideoCaptureConfig {
                fps: 0,
                ..ok.clone()
            },
            VideoCaptureConfig {
                fps: 241,
                ..ok.clone()
            },
            VideoCaptureConfig {
                bitrate_kbps: 0,
                ..ok.clone()
            },
            VideoCaptureConfig {
                max_height: Some(8),
                ..ok.clone()
            },
        ] {
            assert!(matches!(bad.validate(), Err(VideoError::InvalidConfig(_))));
        }
    }

    #[test]
    fn bitrate_validation() {
        assert!(validate_bitrate(1).is_ok());
        assert!(validate_bitrate(1_000_000).is_ok());
        assert!(validate_bitrate(0).is_err());
        assert!(validate_bitrate(1_000_001).is_err());
    }
}
