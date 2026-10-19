//! The encode thread: setup, the fixed-rate tick loop and packet output.

use std::borrow::Cow;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};

use rec_core::media::{VideoCodec, VideoColor, VideoFormat};
use rec_core::replay::Packet;
use rec_core::time::{HNS_PER_SEC, Hns, qpc_now};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
};

use crate::annexb::{self, KeyframeFix, ParameterSets};
use crate::api::{self, CaptureApi, Facts};
use crate::backend::Backend;
use crate::capture::{self, Capture, CaptureFormat, CaptureShared};
use crate::convert::{Conversion, Converter};
use crate::d3d::{self, Gpu};
use crate::dda::{self, DdaCapture};
use crate::display::{self, DisplayColor};
use crate::encoder::EncoderParams;
use crate::geometry::output_size;
use crate::obu::{self, SequenceHeaderCache};
use crate::priority;
use crate::probe::BlackProbe;
use crate::sei;
use crate::util::{ComApartment, Context, Event, Timer, wait_any};
use crate::{
    CodecPreference, HdrMode, PacketSink, SharedStats, VideoCaptureConfig, VideoError, VideoInfo,
};

/// A request from `VideoCapture` to the running encode thread. The thread
/// drains them once per tick (see [`Pipeline::run`]); each carries its own
/// reply channel so the caller can wait for the outcome. Add variants here
/// for further in-place changes.
#[derive(Debug)]
pub(crate) enum Command {
    /// Change the encoder's mean bit rate (kbit/s); replies with the mean
    /// the encoder reports afterwards.
    SetBitrate {
        kbps: u32,
        reply: SyncSender<Result<u32, VideoError>>,
    },
}

/// Marks the pipeline stopped and signals `VideoCapture::stop` when the
/// encode thread ends, including by panic (e.g. in the sink).
struct Finished {
    stats: Arc<SharedStats>,
    done: Sender<()>,
}

impl Drop for Finished {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.stats.set_error("encode thread panicked");
        }
        self.stats.running.store(false, Ordering::Release);
        let _ = self.done.send(());
    }
}

/// Entry point of the encode thread.
pub(crate) fn run(
    config: VideoCaptureConfig,
    sink: PacketSink,
    stats: Arc<SharedStats>,
    stop: Arc<Event>,
    ready: SyncSender<Result<VideoInfo, VideoError>>,
    done: Sender<()>,
    commands: Receiver<Command>,
) {
    // Declared first, so it is dropped last: after COM is uninitialised.
    let _finished = Finished {
        stats: stats.clone(),
        done,
    };
    let _com = match ComApartment::init_mta() {
        Ok(com) => com,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    // Above normal so a busy desktop cannot delay the tick, but never
    // time-critical/realtime: the game always comes first.
    // SAFETY: pseudo-handle of the current thread.
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };

    let mut pipeline = match Pipeline::new(&config, sink, stats.clone(), commands) {
        Ok(p) => p,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    stats.running.store(true, Ordering::Release);
    if ready.send(Ok(pipeline.info.clone())).is_ok() {
        if let Err(e) = pipeline.run(&stop) {
            stats.set_error(e);
        }
    }
    // Teardown (capture first, then encoder, converter, device) happens in
    // field order while COM is still initialised.
    drop(pipeline);
}

struct Pipeline {
    // Field order is drop order: stop capture callbacks first, then the
    // encoder (which returns its input samples), then the NV12 pool.
    _capture: Source,
    encoder: Backend,
    converter: Converter,
    priority: priority::Control,
    shared: Arc<CaptureShared>,
    gpu: Gpu,
    info: VideoInfo,
    out: Output,
    probe: BlackProbe,
    stats: Arc<SharedStats>,
    source: crate::CaptureSource,
    fps: u32,
    duration: Hns,
    commands: Receiver<Command>,
    /// `Latest::seq` of the picture in `last_slot`.
    last_seq: u64,
    last_slot: Option<usize>,
    /// When the encoder last produced output or asked for input; see
    /// [`ENCODER_STALL`].
    progress_at: Option<Hns>,
}

/// An encoder that has taken input and then neither produces output nor
/// asks for more for this long has hung (seen with AMD's AV1 MFT at sizes
/// it cannot encode). Failing turns that into a restart and an error the
/// user sees, instead of a recording that silently stopped.
const ENCODER_STALL: Hns = 3 * HNS_PER_SEC;

/// Turns encoder output into contract-conforming packets for the sink:
/// keyframes found from the bitstream itself, parameter sets / sequence
/// header and HDR10 metadata present in every keyframe.
struct Output {
    sink: PacketSink,
    stats: Arc<SharedStats>,
    codec: VideoCodec,
    /// H.264 / HEVC parameter sets.
    params: ParameterSets,
    /// AV1 sequence header.
    av1: SequenceHeaderCache,
    /// What every keyframe carries for HDR10: an HEVC prefix SEI NAL or AV1
    /// metadata OBUs. Encoders differ in whether (and how) they write it, so
    /// we do it ourselves, the same for every backend.
    hdr_metadata: Option<Vec<u8>>,
    track: rec_core::replay::TrackId,
    segment: u32,
    duration: Hns,
    last_pts: Option<Hns>,
    reported_non_idr: bool,
    reported_order: bool,
    reported_bitstream: bool,
}

impl Output {
    fn new(
        codec: VideoCodec,
        color: VideoColor,
        sequence_header: Option<&[u8]>,
        sink: PacketSink,
        stats: Arc<SharedStats>,
        config: &VideoCaptureConfig,
        duration: Hns,
    ) -> Self {
        let mut params = ParameterSets::new(codec);
        if let (Some(header), false) = (sequence_header, codec == VideoCodec::Av1) {
            params.update_from(header);
        }
        let hdr_metadata = match (color, codec) {
            (VideoColor::Hdr10(meta), VideoCodec::Hevc) => Some(sei::hevc_hdr10_sei(&meta)),
            (VideoColor::Hdr10(meta), VideoCodec::Av1) => Some(obu::hdr10_metadata_obus(&meta)),
            _ => None,
        };
        Self {
            sink,
            stats,
            codec,
            params,
            av1: SequenceHeaderCache::new(),
            hdr_metadata,
            track: config.track,
            segment: config.segment,
            duration,
            last_pts: None,
            reported_non_idr: false,
            reported_order: false,
            reported_bitstream: false,
        }
    }

    /// Records the first problem of a kind without stopping the stream.
    fn report_bitstream(&mut self, what: impl std::fmt::Display) {
        if !self.reported_bitstream {
            self.reported_bitstream = true;
            self.stats.set_error(format!("encoder output: {what}"));
        }
    }

    /// Whether `data` starts a GOP: an IDR picture, or an AV1 key frame
    /// that is shown. The encoder's own flag is only cross-checked.
    fn is_keyframe(&mut self, data: &[u8], clean_point: bool) -> bool {
        let (key, kind) = if self.codec == VideoCodec::Av1 {
            if let Err(e) = self.av1.update_from(data) {
                self.report_bitstream(format_args!("not a valid AV1 temporal unit ({e})"));
            }
            (self.av1.is_key_frame(data) == Some(true), None)
        } else {
            let first_vcl = annexb::first_vcl_type(self.codec, data);
            (
                first_vcl.is_some_and(|t| annexb::is_idr(self.codec, t)),
                first_vcl,
            )
        };
        if clean_point && !key && !self.reported_non_idr {
            self.reported_non_idr = true;
            self.stats.set_error(format!(
                "encoder marked a picture that is not a key frame (NAL type {kind:?}) as a keyframe"
            ));
        }
        key
    }

    /// A keyframe with its parameter sets (or sequence header) and HDR10
    /// metadata in place.
    fn complete_keyframe(&mut self, data: &[u8]) -> Arc<[u8]> {
        let (bytes, fix) = if self.codec == VideoCodec::Av1 {
            match self.av1.ensure_in_key_frame(data) {
                Ok(done) => done,
                Err(e) => {
                    self.report_bitstream(format_args!("not a valid AV1 temporal unit ({e})"));
                    (Cow::Borrowed(data), KeyframeFix::AlreadyPresent)
                }
            }
        } else {
            self.params.ensure_in_keyframe(data)
        };
        match fix {
            KeyframeFix::AlreadyPresent => {}
            KeyframeFix::Inserted => {
                self.stats.ps_inserted.fetch_add(1, Ordering::Relaxed);
            }
            KeyframeFix::Missing => {
                self.stats
                    .set_error("keyframe without parameter sets and none cached");
            }
        }
        let Some(meta) = &self.hdr_metadata else {
            return match bytes {
                Cow::Borrowed(b) => Arc::from(b),
                Cow::Owned(v) => Arc::from(v),
            };
        };
        let with_meta = match self.codec {
            VideoCodec::Hevc => sei::insert_hevc_hdr10_sei(&bytes, meta),
            VideoCodec::Av1 => match obu::insert_hdr10_metadata(&bytes, meta) {
                Ok(done) => done,
                Err(e) => {
                    let message = format!("not a valid AV1 temporal unit ({e})");
                    self.report_bitstream(message);
                    Cow::Borrowed(&*bytes)
                }
            },
            VideoCodec::H264 => Cow::Borrowed(&*bytes),
        };
        Arc::from(&*with_meta)
    }

    fn on_frame(&mut self, data: &[u8], time: Hns, clean_point: bool) {
        let keyframe = self.is_keyframe(data, clean_point);

        if self.last_pts.is_some_and(|prev| time <= prev) && !self.reported_order {
            self.reported_order = true;
            self.stats.set_error(
                "encoder output timestamps are not increasing (B-frames or reordering?)",
            );
        }
        self.last_pts = Some(time);

        let data: Arc<[u8]> = if keyframe {
            self.complete_keyframe(data)
        } else {
            Arc::from(data)
        };
        (self.sink)(Packet {
            track: self.track,
            segment: self.segment,
            pts: time,
            dts: time,
            duration: self.duration,
            keyframe,
            data,
        });
        self.stats.encoded.fetch_add(1, Ordering::Relaxed);
    }
}

impl Pipeline {
    fn new(
        config: &VideoCaptureConfig,
        sink: PacketSink,
        stats: Arc<SharedStats>,
        commands: Receiver<Command>,
    ) -> Result<Self, VideoError> {
        let gpu = d3d::create_for_source(config.source)?;
        let mut notes = Vec::new();
        let display = match display::display_for(config.source) {
            Ok(state) => state.color,
            Err(e) => {
                notes.push(format!("display colour unknown ({e}); recording SDR"));
                None
            }
        };
        let really_hdr = display.is_some_and(|d| d.hdr);
        let display = if config.assume_hdr_display {
            Some(DisplayColor::assume_hdr(display))
        } else {
            display
        };
        let hdr_display = display.filter(|d| d.hdr);
        let plans = color_plans(config.hdr, config.codec, hdr_display, &mut notes);

        let shared = Arc::new(CaptureShared {
            latest: Mutex::default(),
            stats: stats.clone(),
        });
        let mut capture = open_source(
            config,
            &gpu,
            plans[0].0.capture_format(),
            &shared,
            &mut notes,
        )?;
        if capture.api() == CaptureApi::Dda
            && plans[0].0.capture_format() == CaptureFormat::Fp16
            && !really_hdr
        {
            // Measured on Windows 11 26200: an SDR desktop duplicated as FP16
            // holds the sRGB-encoded values, not linear scRGB as from WGC.
            notes.push(
                "Desktop Duplication of an SDR display in FP16 is not linear; the assumed-HDR picture comes out too bright".into(),
            );
        }
        let (source_width, source_height) = capture.size();
        let (width, height) = output_size(source_width, source_height, config.max_height);
        let (encoder, converter, conversion, color) =
            start_encoder(&gpu, config, width, height, &plans, &mut notes)?;
        let picture = (width, height);
        // The encoder may pad the frame for alignment.
        let (width, height) = encoder.coded_size();
        let probe = BlackProbe::new(&gpu.device, width, height, conversion.output_format())?;

        let duration = HNS_PER_SEC / Hns::from(config.fps);
        let info = VideoInfo {
            format: VideoFormat {
                codec: encoder.codec(),
                width,
                height,
                fps: config.fps,
                color,
            },
            capture: capture.api(),
            encoder_name: encoder.name().to_string(),
            adapter_name: gpu.adapter_name.clone(),
            adapter_owns_monitor: gpu.owns_monitor,
            adapter_luid: d3d::luid_key(gpu.luid),
            rate_control: encoder.rate_control().to_string(),
            picture_width: picture.0,
            picture_height: picture.1,
            source_width,
            source_height,
            border_required: capture.border_required(),
            min_update_interval_hns: capture.min_update_interval_hns(),
            display_hdr: hdr_display.is_some(),
            notes,
        };
        let out = Output::new(
            encoder.codec(),
            color,
            encoder.sequence_header(),
            sink,
            stats.clone(),
            config,
            duration,
        );
        let priority = priority::Control::new(&gpu, config.gpu_priority, qpc_now(), stats.clone())?;
        capture.start()?;
        Ok(Self {
            out,
            priority,
            _capture: capture,
            encoder,
            converter,
            probe,
            shared,
            gpu,
            info,
            stats,
            source: config.source,
            fps: config.fps,
            duration,
            commands,
            last_seq: 0,
            last_slot: None,
            progress_at: None,
        })
    }

    /// The tick loop. Returns on stop, source close or error.
    fn run(&mut self, stop: &Event) -> Result<(), VideoError> {
        let timer = Timer::new()?;
        let fps = Hns::from(self.fps);
        let start = qpc_now();
        let tick_time = |n: Hns| start + n * HNS_PER_SEC / fps;
        let mut tick: Hns = 0;
        loop {
            tick += 1;
            let now = qpc_now();
            let mut due = tick_time(tick);
            // Woke up more than a whole period late (system stall): skip the
            // missed ticks instead of bursting to catch up.
            let behind = (now - due) * fps / HNS_PER_SEC;
            if behind >= 1 {
                tick += behind;
                due = tick_time(tick);
                self.stats
                    .dropped
                    .fetch_add(behind as u64, Ordering::Relaxed);
            }
            timer.arm_relative(due - now)?;
            if wait_any(&[stop.handle(), timer.handle()])? == 0 {
                return Ok(());
            }
            if self.stats.failed.load(Ordering::Acquire)
                || self.stats.source_closed.load(Ordering::Acquire)
            {
                // The capture side already recorded why.
                return Ok(());
            }
            self.drain_commands();
            self.tick(due)?;
            #[cfg(debug_assertions)]
            debug_fail(start, due)?;
            if tick % fps == 0 {
                // Once a second: GPU priority, device health and source
                // liveness.
                self.priority.end_second(due);
                // SAFETY: plain status query.
                unsafe { self.gpu.device.GetDeviceRemovedReason() }.ctx("GPU device removed")?;
                if !d3d::source_alive(self.source) {
                    self.stats.source_closed.store(true, Ordering::Release);
                    return Ok(());
                }
            }
        }
    }

    /// Runs the queued [`Command`]s. Called once per tick, right before the
    /// encoder is fed: an empty channel costs one atomic load, and a rate
    /// change lands between two pictures, so it never races the encoder. A
    /// failed command is reported to its caller only; it does not stop the
    /// pipeline.
    fn drain_commands(&mut self) {
        loop {
            match self.commands.try_recv() {
                Ok(Command::SetBitrate { kbps, reply }) => {
                    // The caller may have timed out and gone; nothing to do.
                    let _ = reply.send(self.encoder.set_bitrate(kbps));
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            }
        }
    }

    fn tick(&mut self, time: Hns) -> Result<(), VideoError> {
        let out = &mut self.out;
        let encoded = self.stats.encoded.load(Ordering::Relaxed);
        self.encoder
            .poll(|data, t, key| out.on_frame(data, t, key))?;
        self.probe.poll(&self.gpu.context, time, &self.stats);
        if self.encoder.wants_input() || self.stats.encoded.load(Ordering::Relaxed) != encoded {
            self.progress_at = Some(time);
        } else if self.progress_at.is_some_and(|at| time - at > ENCODER_STALL) {
            return Err(VideoError::Other(format!(
                "the encoder ({}) stopped responding",
                self.encoder.name()
            )));
        }

        let latest = self
            .shared
            .latest
            .lock()
            .map_err(|_| VideoError::Other("capture state lock poisoned".into()))?;
        if latest.seq == 0 {
            // Nothing captured yet; the stream starts with the first frame.
            return Ok(());
        }
        // GPU work queued 3 periods ago and not done yet: the tick is
        // starved (frames are about to drop) even when its own frame
        // still makes it.
        let late = self
            .priority
            .late_work(&self.gpu.context, time, self.duration);
        if !self.encoder.wants_input() {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            self.priority.tick(true);
            return Ok(());
        }
        let fresh = latest.seq != self.last_seq;
        let slot = match self.last_slot {
            // Nothing new and the previous NV12 picture is back from the
            // encoder: resubmit it without touching the GPU.
            Some(s) if !fresh && self.converter.is_free(s) => s,
            _ => match self.converter.free_slot(self.last_slot) {
                Some(s) => {
                    self.converter.blit(&latest, s)?;
                    self.priority.fence(&self.gpu.context, time);
                    self.probe
                        .offer(&self.gpu.context, self.converter.texture(s), time);
                    s
                }
                None => {
                    self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                    self.priority.tick(true);
                    return Ok(());
                }
            },
        };
        self.last_seq = latest.seq;
        drop(latest);
        self.last_slot = Some(slot);

        if self
            .encoder
            .submit(&self.converter, slot, time, self.duration)?
        {
            self.stats.submitted.fetch_add(1, Ordering::Relaxed);
            if !fresh {
                self.stats.repeated.fetch_add(1, Ordering::Relaxed);
            }
            self.priority.tick(late);
        } else {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            self.priority.tick(true);
        }
        Ok(())
    }
}

/// The capture side of a pipeline: either API fills the same "latest"
/// texture in the same format.
#[derive(Debug)]
enum Source {
    Wgc(Capture),
    Dda(DdaCapture),
}

impl Source {
    fn api(&self) -> CaptureApi {
        match self {
            Source::Wgc(_) => CaptureApi::Wgc,
            Source::Dda(_) => CaptureApi::Dda,
        }
    }

    fn size(&self) -> (u32, u32) {
        match self {
            Source::Wgc(c) => (c.source_width, c.source_height),
            Source::Dda(c) => (c.source_width, c.source_height),
        }
    }

    fn border_required(&self) -> Option<bool> {
        match self {
            Source::Wgc(c) => c.border_required,
            Source::Dda(_) => None,
        }
    }

    fn min_update_interval_hns(&self) -> Option<i64> {
        match self {
            Source::Wgc(c) => c.min_update_interval_hns,
            Source::Dda(c) => Some(c.min_update_interval_hns),
        }
    }

    fn start(&mut self) -> Result<(), VideoError> {
        match self {
            Source::Wgc(c) => c.start(),
            Source::Dda(c) => c.start(),
        }
    }
}

/// Creates the capture source with the API `config.capture` and the system
/// call for (see [`crate::api`]).
fn open_source(
    config: &VideoCaptureConfig,
    gpu: &Gpu,
    format: CaptureFormat,
    shared: &Arc<CaptureShared>,
    notes: &mut Vec<String>,
) -> Result<Source, VideoError> {
    let dda_usable = dda::usable(gpu, d3d::target_monitor(config.source));
    let facts = Facts {
        wgc: capture::supported(),
        border_property: capture::border_property_present(),
        dda: dda_usable,
    };
    let dda =
        || DdaCapture::new(config.source, gpu, config.fps, format, shared.clone()).map(Source::Dda);
    match api::first_choice(config.capture, facts) {
        CaptureApi::Dda => dda(),
        _ => {
            let wgc = Capture::new(
                config.source,
                gpu,
                config.fps,
                config.cursor,
                format,
                shared.clone(),
            )?;
            if api::dda_instead(config.capture, wgc.border_required, dda_usable) {
                // Not started yet, so the border was never shown.
                drop(wgc);
                notes.push(
                    "Windows.Graphics.Capture could not turn off the yellow border; capturing with Desktop Duplication".into(),
                );
                return dda();
            }
            Ok(Source::Wgc(wgc))
        }
    }
}

/// The (conversion, encoded colour) choices to try, best first. HDR10 needs
/// a 10-bit codec and encoder; the tone-map to SDR always works from the
/// same FP16 capture, so it is the fallback.
fn color_plans(
    mode: HdrMode,
    codec: CodecPreference,
    hdr_display: Option<DisplayColor>,
    notes: &mut Vec<String>,
) -> Vec<(Conversion, VideoColor)> {
    let Some(display) = hdr_display.filter(|_| mode != HdrMode::Off) else {
        return vec![(Conversion::Sdr, VideoColor::Sdr)];
    };
    let tonemap = (
        Conversion::Tonemap {
            sdr_white_nits: display.sdr_white_nits,
        },
        VideoColor::Sdr,
    );
    match (mode, codec) {
        (HdrMode::Auto, CodecPreference::H264) => {
            notes.push("H.264 cannot carry HDR10; tone-mapping HDR to SDR".into());
            vec![tonemap]
        }
        (HdrMode::Auto, _) => vec![
            (
                Conversion::Hdr10 {
                    peak_nits: display.peak_nits(),
                },
                VideoColor::Hdr10(display.metadata()),
            ),
            tonemap,
        ],
        _ => vec![tonemap],
    }
}

/// Creates the encoder and converter for the first plan that works.
fn start_encoder(
    gpu: &Gpu,
    config: &VideoCaptureConfig,
    width: u32,
    height: u32,
    plans: &[(Conversion, VideoColor)],
    notes: &mut Vec<String>,
) -> Result<(Backend, Converter, Conversion, VideoColor), VideoError> {
    let mut last = None;
    for &(conversion, color) in plans {
        let params = EncoderParams {
            width,
            height,
            fps: config.fps,
            bitrate_kbps: config.bitrate_kbps,
            color,
        };
        let started = Backend::create(gpu, config.encoder, config.codec, &params, notes).and_then(
            |encoder| {
                let converter = Converter::new(
                    &gpu.device,
                    encoder.coded_size(),
                    (width, height),
                    config.fps,
                    conversion,
                )?;
                Ok((encoder, converter))
            },
        );
        match started {
            Ok((encoder, converter)) => return Ok((encoder, converter, conversion, color)),
            Err(e) => {
                if color.is_hdr() {
                    notes.push(format!("HDR10 recording failed ({e}); tone-mapping to SDR"));
                }
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| VideoError::Other("no colour plan".into())))
}

/// Debug builds only: with `REC_DEBUG_VIDEO_FAIL_AFTER=<seconds>` set, the
/// first pipeline in the process fails that long after it started, the way a
/// lost GPU device does. A real device loss needs a driver reset (TDR), so
/// this is how recd's recovery is tested by hand.
#[cfg(debug_assertions)]
fn debug_fail(start: Hns, now: Hns) -> Result<(), VideoError> {
    use std::sync::atomic::AtomicBool;
    static FAILED: AtomicBool = AtomicBool::new(false);
    let after = std::env::var("REC_DEBUG_VIDEO_FAIL_AFTER")
        .ok()
        .and_then(|v| v.parse::<f64>().ok());
    match after {
        Some(after)
            if now - start >= (after * HNS_PER_SEC as f64) as Hns
                && !FAILED.swap(true, Ordering::Relaxed) =>
        {
            Err(VideoError::Other(
                "simulated device loss (REC_DEBUG_VIDEO_FAIL_AFTER)".into(),
            ))
        }
        _ => Ok(()),
    }
}

impl std::fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipeline")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}
