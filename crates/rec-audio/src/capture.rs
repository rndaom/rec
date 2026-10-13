//! The capture thread: WASAPI -> timeline -> AAC encoder -> sink.
//!
//! Capture and encoding share one MMCSS thread. Encoding is cheap (the MF AAC
//! encoder runs at ~170x real time on music here), so a second thread would
//! only add a context switch and a queue hop per packet.
//!
//! The thread polls every [`POLL_MS`] instead of waking on the WASAPI event
//! (every 10 ms engine period). Measured on a 10 s loopback capture of music:
//! event-driven 1.31% of one core, 20 ms polling 1.10%, 40 ms polling 0.98%;
//! the saving is mostly fewer cold-cache encoder calls. The 200 ms WASAPI
//! buffer leaves ample headroom, and the extra latency is irrelevant for a
//! replay buffer. Polling also covers loopback (which signals nothing while
//! nothing plays) and a missing device with the same code path.

use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use rec_core::media::AudioFormat;
use rec_core::replay::{Packet, TrackId};
use rec_core::time::{Hns, millis, qpc_now};
use windows::Win32::Foundation::{WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::IMMDeviceEnumerator;
use windows::Win32::System::Threading::WaitForMultipleObjects;

use crate::encoder::AacEncoder;
use crate::sys::{ComGuard, Event, Mmcss};
use crate::timeline::{Timeline, TimelineConfig};
use crate::wasapi::{self, Chunk, Notifications, Stream};
use crate::{
    AudioCaptureConfig, AudioError, AudioSource, AudioStats, ENCODER_DELAY_SAMPLES, SAMPLE_RATE,
    SAMPLES_PER_PACKET,
};

/// Poll period. With the default 15.6 ms timer resolution the thread actually
/// wakes every ~47 ms; [`TimelineConfig::idle_margin`] must stay above that
/// plus one engine period.
const POLL_MS: u32 = 40;
/// Retry period for opening a device while none is available.
const RETRY: Hns = millis(2_000);

type Sink = Box<dyn FnMut(Packet) + Send>;

/// A running audio capture. Dropping it stops the thread and joins it.
#[derive(Debug)]
pub struct AudioCapture {
    format: AudioFormat,
    bitrate_kbps: u32,
    stats: Arc<Mutex<AudioStats>>,
    stop: Arc<Event>,
    thread: Option<JoinHandle<()>>,
}

impl AudioCapture {
    /// Starts capturing. Returns once the encoder is configured (so
    /// [`format`](Self::format) is known). A missing device is not an error:
    /// the track carries silence until one appears, see [`AudioStats::device`].
    pub fn start(config: AudioCaptureConfig, sink: Sink) -> Result<Self, AudioError> {
        let stop = Arc::new(Event::new()?);
        let stats = Arc::new(Mutex::new(AudioStats {
            running: true,
            ..AudioStats::default()
        }));
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name(format!("rec-audio-{}", config.source.label()))
            .spawn({
                let stop = Arc::clone(&stop);
                let stats = Arc::clone(&stats);
                move || {
                    // Clears `running` however the thread ends, panics included.
                    let _running = Running(&stats);
                    let result = run(config, sink, &stop, &stats, &ready_tx);
                    if let Err(e) = result {
                        let mut s = lock(&stats);
                        s.last_error = Some(e.to_string());
                        // Only delivered if start() is still waiting.
                        let _ = ready_tx.try_send(Err(e));
                    }
                }
            })
            .map_err(|e| AudioError::Thread(e.to_string()))?;
        match ready_rx.recv() {
            Ok(Ok((format, bitrate_kbps))) => Ok(Self {
                format,
                bitrate_kbps,
                stats,
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(AudioError::Thread(
                    "capture thread exited during start".into(),
                ))
            }
        }
    }

    /// The track format: AAC-LC, 48 kHz, stereo, with its AudioSpecificConfig.
    pub fn format(&self) -> &AudioFormat {
        &self.format
    }

    /// The bitrate actually used (the requested one mapped to a supported
    /// value).
    pub fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps
    }

    pub fn stats(&self) -> AudioStats {
        lock(&self.stats).clone()
    }

    /// Stops capture, flushes the encoder into the sink and joins the thread.
    pub fn stop(self) {}
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.set();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Running<'a>(&'a Mutex<AudioStats>);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        lock(self.0).running = false;
    }
}

fn lock(stats: &Mutex<AudioStats>) -> MutexGuard<'_, AudioStats> {
    stats.lock().unwrap_or_else(PoisonError::into_inner)
}

type Ready = mpsc::SyncSender<Result<(AudioFormat, u32), AudioError>>;

fn run(
    config: AudioCaptureConfig,
    sink: Sink,
    stop: &Event,
    stats: &Mutex<AudioStats>,
    ready: &Ready,
) -> Result<(), AudioError> {
    let _com = ComGuard::mta()?;
    let _mmcss = Mmcss::audio();
    let encoder = AacEncoder::new(config.bitrate_kbps)?;
    let enumr = wasapi::enumerator()?;
    let device_event = Arc::new(Event::new()?);
    let _notifications =
        Notifications::register(&enumr, &config.source, Arc::clone(&device_event))?;

    let mut cap = Capture {
        source: config.source,
        enumr,
        stream: None,
        last_attempt: 0,
        opened_once: false,
        timeline: Timeline::new(qpc_now(), TimelineConfig::default()),
        encoder,
        out: Output {
            sink,
            track: config.track,
            next_packet: 0,
            packets: 0,
        },
        stats,
    };
    cap.try_open();
    let _ = ready.send(Ok((
        cap.encoder.format().clone(),
        cap.encoder.bitrate_kbps(),
    )));

    let handles = [stop.handle(), device_event.handle()];
    let device_changed = WAIT_EVENT(WAIT_OBJECT_0.0 + 1);
    loop {
        // SAFETY: the handles are live events owned by this frame or by
        // `AudioCapture`, which joins this thread before closing them.
        let wait = unsafe { WaitForMultipleObjects(&handles, false, POLL_MS) };
        if wait == WAIT_OBJECT_0 {
            break;
        }
        if wait == device_changed {
            cap.on_device_change();
        } else if wait != WAIT_TIMEOUT {
            // WAIT_FAILED cannot happen with valid handles; don't spin if it
            // somehow does.
            std::thread::sleep(std::time::Duration::from_millis(POLL_MS.into()));
        }
        cap.tick()?;
    }
    cap.finish()
}

struct Output {
    sink: Sink,
    track: TrackId,
    /// Index of the next packet the encoder emits.
    next_packet: i64,
    packets: u64,
}

impl Output {
    fn emit(&mut self, timeline: &Timeline, data: &[u8]) {
        let len = i64::from(SAMPLES_PER_PACKET);
        // Packet i decodes to input samples starting at i * 1024 - delay.
        let first = self.next_packet * len - ENCODER_DELAY_SAMPLES;
        let pts = timeline.time_of(first);
        self.next_packet += 1;
        self.packets += 1;
        (self.sink)(Packet {
            track: self.track,
            segment: 0,
            pts,
            dts: pts,
            duration: timeline.duration_of(first, len),
            keyframe: true,
            data: Arc::from(data),
        });
    }
}

struct Capture<'a> {
    source: AudioSource,
    enumr: IMMDeviceEnumerator,
    stream: Option<Stream>,
    /// QPC time of the last open attempt while no stream is running.
    last_attempt: Hns,
    opened_once: bool,
    timeline: Timeline,
    encoder: AacEncoder,
    out: Output,
    stats: &'a Mutex<AudioStats>,
}

impl Capture<'_> {
    /// One wake-up: read what the device delivered, fill silence for what it
    /// did not, retry a missing device now and then, publish stats.
    fn tick(&mut self) -> Result<(), AudioError> {
        self.read_stream()?;
        let now = qpc_now();
        if self.stream.is_none() && now - self.last_attempt >= RETRY {
            self.try_open();
        }
        let silence = self.timeline.on_idle(now);
        self.feed_silence(silence)?;
        self.publish();
        Ok(())
    }

    fn finish(mut self) -> Result<(), AudioError> {
        self.read_stream()?;
        let silence = self.timeline.on_finish(qpc_now());
        self.feed_silence(silence)?;
        let Self {
            encoder,
            out,
            timeline,
            ..
        } = &mut self;
        encoder.drain(&mut |data| out.emit(timeline, data))?;
        self.stream = None;
        self.publish();
        Ok(())
    }

    fn feed_silence(&mut self, frames: u64) -> Result<(), AudioError> {
        if frames == 0 {
            return Ok(());
        }
        let Self {
            encoder,
            out,
            timeline,
            ..
        } = self;
        encoder.push_silence(frames, &mut |data| out.emit(timeline, data))
    }

    /// Drains the stream. Device loss is handled here (the stream is dropped
    /// and reopened); only encoder failures are returned.
    fn read_stream(&mut self) -> Result<(), AudioError> {
        let Self {
            stream,
            encoder,
            out,
            timeline,
            ..
        } = self;
        let Some(stream) = stream else {
            return Ok(());
        };
        let mut encode_error = false;
        let now = qpc_now();
        let result = stream.read(|chunk: Chunk<'_>| {
            if chunk.discontinuity {
                timeline.mark_discontinuity();
            }
            let place = timeline.on_data(chunk.time, chunk.frames, now);
            let mut emit = |data: &[u8]| out.emit(timeline, data);
            let r = encoder
                .push_silence(place.silence_before, &mut emit)
                .and_then(|()| {
                    let kept = u64::from(chunk.frames - place.skip);
                    match chunk.pcm {
                        Some(pcm) => encoder.push_pcm(&pcm[place.skip as usize * 2..], &mut emit),
                        None => encoder.push_silence(kept, &mut emit),
                    }
                });
            encode_error = r.is_err();
            r
        });
        match result {
            Ok(()) => Ok(()),
            Err(e) if encode_error => Err(e),
            Err(e) => {
                // The device is gone (AUDCLNT_E_DEVICE_INVALIDATED), the audio
                // service restarted, or similar: drop it and try the current
                // device; silence fills the gap meanwhile.
                self.record_error(&e);
                self.close_stream();
                self.try_open();
                Ok(())
            }
        }
    }

    fn close_stream(&mut self) {
        if self.stream.take().is_some() {
            lock(self.stats).device = None;
        }
        self.timeline.mark_discontinuity();
    }

    fn on_device_change(&mut self) {
        let switch = match &self.stream {
            None => true,
            Some(stream) => {
                self.source.follows_default()
                    && wasapi::current_device_id(&self.enumr, &self.source)
                        .is_some_and(|id| id != stream.id)
            }
        };
        if switch {
            self.close_stream();
            self.try_open();
        }
    }

    fn try_open(&mut self) {
        self.last_attempt = qpc_now();
        match Stream::open(&self.enumr, &self.source) {
            Ok(stream) => {
                let mut s = lock(self.stats);
                if self.opened_once {
                    s.restarts += 1;
                }
                s.device = Some(stream.name.clone().unwrap_or_else(|| stream.id.clone()));
                drop(s);
                self.opened_once = true;
                self.stream = Some(stream);
                self.timeline.mark_discontinuity();
            }
            Err(e) => {
                self.record_error(&e);
            }
        }
    }

    fn record_error(&self, e: &AudioError) {
        lock(self.stats).last_error = Some(e.to_string());
    }

    fn publish(&self) {
        let t = self.timeline.stats();
        let per_ms = u64::from(SAMPLE_RATE / 1000);
        let mut s = lock(self.stats);
        s.packets = self.out.packets;
        s.silence_ms = t.silence_samples / per_ms;
        s.dropped_ms = t.dropped_samples / per_ms;
        s.drift_corrections = t.drift_corrections;
        s.long_gaps = t.long_gaps;
    }
}
