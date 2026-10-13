//! The encode thread: setup, the fixed-rate tick loop and packet output.

use std::borrow::Cow;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Sender, SyncSender};
use std::sync::{Arc, Mutex};

use rec_core::media::{VideoCodec, VideoFormat};
use rec_core::replay::Packet;
use rec_core::time::{HNS_PER_SEC, Hns, qpc_now};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
};

use crate::annexb::{self, KeyframeFix, ParameterSets};
use crate::capture::{Capture, CaptureShared};
use crate::convert::Converter;
use crate::d3d::{self, Gpu};
use crate::encoder::{Encoder, EncoderParams};
use crate::geometry::output_size;
use crate::probe::BlackProbe;
use crate::util::{ComApartment, Context, Event, Timer, wait_any};
use crate::{PacketSink, SharedStats, VideoCaptureConfig, VideoError, VideoInfo};

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

    let mut pipeline = match Pipeline::new(&config, sink, stats.clone()) {
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
    _capture: Capture,
    encoder: Encoder,
    converter: Converter,
    shared: Arc<CaptureShared>,
    gpu: Gpu,
    info: VideoInfo,
    out: Output,
    probe: BlackProbe,
    stats: Arc<SharedStats>,
    source: crate::CaptureSource,
    fps: u32,
    duration: Hns,
    /// `Latest::seq` of the picture in `last_slot`.
    last_seq: u64,
    last_slot: Option<usize>,
}

/// Turns encoder output into contract-conforming packets for the sink.
struct Output {
    sink: PacketSink,
    stats: Arc<SharedStats>,
    codec: VideoCodec,
    params: ParameterSets,
    track: rec_core::replay::TrackId,
    segment: u32,
    duration: Hns,
    last_pts: Option<Hns>,
    reported_non_idr: bool,
    reported_order: bool,
}

impl Output {
    fn on_frame(&mut self, data: &[u8], time: Hns, clean_point: bool) {
        let first_vcl = annexb::first_vcl_type(self.codec, data);
        let idr = first_vcl.is_some_and(|t| annexb::is_idr(self.codec, t));
        if clean_point && !idr && !self.reported_non_idr {
            self.reported_non_idr = true;
            self.stats.set_error(format!(
                "encoder marked a non-IDR picture (NAL type {first_vcl:?}) as a keyframe"
            ));
        }
        let keyframe = idr;

        if self.last_pts.is_some_and(|prev| time <= prev) && !self.reported_order {
            self.reported_order = true;
            self.stats.set_error(
                "encoder output timestamps are not increasing (B-frames or reordering?)",
            );
        }
        self.last_pts = Some(time);

        let data: Arc<[u8]> = if keyframe {
            let (bytes, fix) = self.params.ensure_in_keyframe(data);
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
            match bytes {
                Cow::Borrowed(b) => Arc::from(b),
                Cow::Owned(v) => Arc::from(v),
            }
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
    ) -> Result<Self, VideoError> {
        let gpu = d3d::create_for_source(config.source)?;
        let shared = Arc::new(CaptureShared {
            latest: Mutex::default(),
            stats: stats.clone(),
        });
        let capture = Capture::new(
            config.source,
            &gpu,
            config.fps,
            config.cursor,
            shared.clone(),
        )?;
        let (width, height) = output_size(
            capture.source_width,
            capture.source_height,
            config.max_height,
        );
        let params = EncoderParams {
            width,
            height,
            fps: config.fps,
            bitrate_kbps: config.bitrate_kbps,
        };
        let encoder = Encoder::new(
            &gpu.device,
            gpu.luid,
            &gpu.adapter_name,
            config.codec,
            &params,
        )?;
        let converter = Converter::new(&gpu.device, width, height, config.fps)?;
        let probe = BlackProbe::new(&gpu.device, width, height)?;

        let mut param_sets = ParameterSets::new(encoder.codec);
        if let Some(header) = &encoder.sequence_header {
            param_sets.update_from(header);
        }
        let duration = HNS_PER_SEC / Hns::from(config.fps);
        let info = VideoInfo {
            format: VideoFormat {
                codec: encoder.codec,
                width,
                height,
                fps: config.fps,
            },
            encoder_name: encoder.name.clone(),
            adapter_name: gpu.adapter_name.clone(),
            adapter_owns_monitor: gpu.owns_monitor,
            adapter_luid: d3d::luid_key(gpu.luid),
            rate_control: encoder.rate_control.clone(),
            source_width: capture.source_width,
            source_height: capture.source_height,
            border_required: capture.border_required,
            min_update_interval_hns: capture.min_update_interval_hns,
        };
        capture.start()?;
        Ok(Self {
            out: Output {
                sink,
                stats: stats.clone(),
                codec: encoder.codec,
                params: param_sets,
                track: config.track,
                segment: config.segment,
                duration,
                last_pts: None,
                reported_non_idr: false,
                reported_order: false,
            },
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
            last_seq: 0,
            last_slot: None,
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
            self.tick(due)?;
            #[cfg(debug_assertions)]
            debug_fail(start, due)?;
            if tick % fps == 0 {
                // Once a second: device health and source liveness.
                // SAFETY: plain status query.
                unsafe { self.gpu.device.GetDeviceRemovedReason() }.ctx("GPU device removed")?;
                if !d3d::source_alive(self.source) {
                    self.stats.source_closed.store(true, Ordering::Release);
                    return Ok(());
                }
            }
        }
    }

    fn tick(&mut self, time: Hns) -> Result<(), VideoError> {
        let out = &mut self.out;
        self.encoder
            .poll(|data, t, key| out.on_frame(data, t, key))?;
        self.probe.poll(&self.gpu.context, time, &self.stats);

        let latest = self
            .shared
            .latest
            .lock()
            .map_err(|_| VideoError::Other("capture state lock poisoned".into()))?;
        if latest.seq == 0 {
            // Nothing captured yet; the stream starts with the first frame.
            return Ok(());
        }
        if !self.encoder.wants_input() {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
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
                    self.probe
                        .offer(&self.gpu.context, self.converter.texture(s), time);
                    s
                }
                None => {
                    self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
            },
        };
        self.last_seq = latest.seq;
        drop(latest);
        self.last_slot = Some(slot);

        let sample = self.converter.take_sample(slot)?;
        // SAFETY: plain setters on our own sample.
        unsafe {
            sample.SetSampleTime(time).ctx("SetSampleTime")?;
            sample
                .SetSampleDuration(self.duration)
                .ctx("SetSampleDuration")?;
        }
        if self.encoder.submit(&sample)? {
            self.stats.submitted.fetch_add(1, Ordering::Relaxed);
            if !fresh {
                self.stats.repeated.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
        }
        // Dropping our reference returns the sample to the pool as soon as
        // the encoder is done with it.
        drop(sample);
        Ok(())
    }
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
