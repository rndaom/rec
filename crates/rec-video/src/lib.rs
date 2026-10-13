//! rec's video pipeline: Windows.Graphics.Capture → D3D11 video processor
//! (BGRA → NV12, scale, letterbox) → Media Foundation hardware encoder →
//! Annex-B [`Packet`]s that follow the contract in [`rec_core::media`].
//!
//! Frames never leave the GPU. Per captured frame the GPU does one
//! `CopyResource`; per encode tick it does one `VideoProcessorBlt` (skipped
//! when the previous NV12 frame can simply be resubmitted) and the hardware
//! encode. No thread spins: the encode thread sleeps on a high-resolution
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
pub mod geometry;
mod probe;

#[cfg(windows)]
mod capture;
#[cfg(windows)]
mod convert;
#[cfg(windows)]
mod d3d;
#[cfg(windows)]
mod encoder;
#[cfg(windows)]
mod pipeline;
#[cfg(windows)]
mod util;

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoCaptureConfig {
    pub source: CaptureSource,
    /// Encode tick rate (constant frame rate), 1..=240.
    pub fps: u32,
    /// Downscale so the encoded height is at most this (aspect ratio kept).
    pub max_height: Option<u32>,
    pub codec: CodecPreference,
    /// Mean bitrate in kbit/s.
    pub bitrate_kbps: u32,
    /// Draw the mouse cursor into the capture.
    pub cursor: bool,
    pub track: TrackId,
    pub segment: u32,
}

impl VideoCaptureConfig {
    /// Defaults: 60 fps, native size, codec auto, 20 Mbit/s, cursor on,
    /// video track 0, segment 0.
    pub fn new(source: CaptureSource) -> Self {
        Self {
            source,
            fps: 60,
            max_height: None,
            codec: CodecPreference::Auto,
            bitrate_kbps: 20_000,
            cursor: true,
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
        if self.bitrate_kbps == 0 || self.bitrate_kbps > 1_000_000 {
            return Err(VideoError::InvalidConfig(format!(
                "bitrate {} kbit/s is outside 1..=1000000",
                self.bitrate_kbps
            )));
        }
        if self.max_height.is_some_and(|h| h < 16) {
            return Err(VideoError::InvalidConfig(
                "max_height must be at least 16".into(),
            ));
        }
        Ok(())
    }
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
    /// Size of the newest captured frame. It follows the source (a window
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

/// Facts about the running pipeline, fixed for the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoInfo {
    pub format: VideoFormat,
    /// Friendly name of the Media Foundation encoder MFT.
    pub encoder_name: String,
    /// DXGI description of the adapter the capture/encode device lives on.
    pub adapter_name: String,
    /// The adapter is the one whose output shows the captured monitor.
    pub adapter_owns_monitor: bool,
    /// The adapter's LUID; compare with [`adapter_for`] to notice that the
    /// source moved to another GPU.
    pub adapter_luid: i64,
    /// Rate-control mode the encoder reported after configuration.
    pub rate_control: String,
    /// Source size when the session started.
    pub source_width: u32,
    pub source_height: u32,
    /// Whether Windows draws the yellow capture border: `Some(false)` when it
    /// was turned off, `None` when the OS has no `IsBorderRequired` property.
    pub border_required: Option<bool>,
    /// `MinUpdateInterval` applied to the capture session, if supported.
    pub min_update_interval_hns: Option<i64>,
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

/// A running capture → encode pipeline. Dropping it stops and joins it.
#[derive(Debug)]
pub struct VideoCapture {
    info: VideoInfo,
    stats: Arc<SharedStats>,
    #[cfg(windows)]
    stop: Arc<util::Event>,
    /// Signalled (or disconnected) when the encode thread has finished.
    done: Option<std::sync::mpsc::Receiver<()>>,
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
        let thread = {
            let stats = stats.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("rec-video-encode".into())
                .spawn(move || pipeline::run(config, sink, stats, stop, ready_tx, done_tx))
                .map_err(|e| VideoError::Other(format!("cannot spawn encode thread: {e}")))?
        };
        match ready_rx.recv() {
            Ok(Ok(info)) => Ok(Self {
                info,
                stats,
                stop,
                done: Some(done_rx),
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
}
