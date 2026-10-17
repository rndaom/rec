//! The capture thread: WASAPI inputs -> timelines -> AAC encoders (and the
//! mixer) -> sink.
//!
//! Every input, its encoder and the mixed track share one MMCSS thread.
//! Encoding is cheap (the MF AAC encoder runs at ~170x real time on music
//! here), so more threads would only add context switches, and one thread
//! makes the mix a plain function call with no queues between inputs.
//!
//! Endpoint streams (system loopback, microphones) are polled every
//! [`POLL_MS`] instead of waking on the WASAPI event (every 10 ms engine
//! period). Measured on a 10 s loopback capture of music: event-driven 1.31%
//! of one core, 20 ms polling 1.10%, 40 ms polling 0.98%; the saving is mostly
//! fewer cold-cache encoder calls. The 200 ms WASAPI buffer leaves ample
//! headroom, and the extra latency is irrelevant for a replay buffer. Polling
//! also covers loopback (which signals nothing while nothing plays) and a
//! missing device with the same code path.
//!
//! Process loopback streams cannot be polled that slowly: their queue holds
//! only ~30 ms (polled every 40 ms, a quarter of the audio was lost). They are
//! opened event-driven, and the thread also wakes on their events; every
//! wake-up drains every input.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use rec_core::media::AudioFormat;
use rec_core::replay::{Packet, TrackId};
use rec_core::time::{Hns, millis, qpc_now};
use windows::Win32::Foundation::{HANDLE, WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::IMMDeviceEnumerator;
use windows::Win32::System::Threading::WaitForMultipleObjects;

use crate::encoder::AacEncoder;
use crate::levels::{Levels, Meter};
use crate::mixer::{Mixed, Mixer};
use crate::process::{self, Followed};
use crate::sys::{ComGuard, Event, Mmcss};
use crate::timeline::{Timeline, TimelineConfig, hns_to_samples, samples_to_hns};
use crate::wasapi::{self, Chunk, Notifications, Stream};
use crate::{
    AudioCaptureConfig, AudioError, AudioSource, AudioStats, ENCODER_DELAY_SAMPLES, InputStats,
    SAMPLES_PER_PACKET,
};

/// Poll period. With the default 15.6 ms timer resolution the thread actually
/// wakes every ~47 ms; [`TimelineConfig::idle_margin`] must stay above that
/// plus one engine period.
const POLL_MS: u32 = 40;
/// Retry period for opening a device, or finding the followed app, while
/// none is available.
const RETRY: Hns = millis(2_000);
/// Stats are published at most this often (process loopback wakes the thread
/// every 10 ms).
const PUBLISH_EVERY: Hns = millis(250);
/// Largest block the mixer hands to the mixed track's encoder at once.
const MIX_BLOCK: usize = 4_800;

type Sink = Box<dyn FnMut(Packet) + Send>;

/// A running audio capture. Dropping it stops the thread and joins it.
#[derive(Debug)]
pub struct AudioCapture {
    format: AudioFormat,
    bitrate_kbps: u32,
    tracks: Vec<TrackId>,
    stats: Arc<Mutex<AudioStats>>,
    levels: Levels,
    /// One flag per input, in config order.
    muted: Vec<Arc<AtomicBool>>,
    stop: Arc<Event>,
    thread: Option<JoinHandle<()>>,
}

impl AudioCapture {
    /// Starts capturing. Returns once the encoders are configured (so
    /// [`format`](Self::format) is known). A missing device or app is not an
    /// error: that input carries silence until it appears, see
    /// [`InputStats::device`].
    pub fn start(config: AudioCaptureConfig, sink: Sink) -> Result<Self, AudioError> {
        config.validate()?;
        let tracks = config.tracks();
        let muted: Vec<_> = config
            .inputs
            .iter()
            .map(|_| Arc::new(AtomicBool::new(false)))
            .collect();
        let stop = Arc::new(Event::new()?);
        let stats = Arc::new(Mutex::new(AudioStats {
            running: true,
            inputs: vec![InputStats::default(); config.inputs.len()],
            ..AudioStats::default()
        }));
        let levels = Levels::default();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("rec-audio".into())
            .spawn({
                let stop = Arc::clone(&stop);
                let stats = Arc::clone(&stats);
                let levels = levels.clone();
                let muted = muted.clone();
                move || {
                    // Clears `running` however the thread ends, panics included.
                    let _running = Running(&stats);
                    let result = run(config, sink, &muted, &stop, &stats, &levels, &ready_tx);
                    if let Err(e) = result {
                        lock(&stats).last_error = Some(e.to_string());
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
                tracks,
                stats,
                levels,
                muted,
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

    /// The format of every track: AAC-LC, 48 kHz, stereo, with its
    /// AudioSpecificConfig.
    pub fn format(&self) -> &AudioFormat {
        &self.format
    }

    /// The bitrate actually used (the requested one mapped to a supported
    /// value).
    pub fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps
    }

    /// The tracks written, mixed track first.
    pub fn tracks(&self) -> &[TrackId] {
        &self.tracks
    }

    pub fn stats(&self) -> AudioStats {
        lock(&self.stats).clone()
    }

    /// Loudness of every track, 10 values per second. Shared with the capture
    /// thread: the handle keeps filling as long as the capture runs.
    pub fn levels(&self) -> Levels {
        self.levels.clone()
    }

    /// Mutes or unmutes input `input` (an index into
    /// [`AudioCaptureConfig::inputs`]) from its next chunk on: a muted input
    /// carries silence on its own track and adds nothing to the mix, so its
    /// track and the timeline stay intact and nothing has to restart.
    pub fn set_muted(&self, input: usize, muted: bool) {
        if let Some(flag) = self.muted.get(input) {
            flag.store(muted, Ordering::Relaxed);
        }
    }

    pub fn is_muted(&self, input: usize) -> bool {
        self.muted
            .get(input)
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    /// Stops capture, flushes the encoders into the sink and joins the thread.
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

/// Grid index (see [`crate::mixer`]) of the sample that plays at `t`.
fn grid_index(t: Hns, epoch: Hns) -> i64 {
    let d = t - epoch;
    if d >= 0 {
        hns_to_samples(d) as i64
    } else {
        -(hns_to_samples(-d) as i64)
    }
}

fn run(
    config: AudioCaptureConfig,
    mut sink: Sink,
    muted: &[Arc<AtomicBool>],
    stop: &Event,
    stats: &Mutex<AudioStats>,
    levels: &Levels,
    ready: &Ready,
) -> Result<(), AudioError> {
    let _com = ComGuard::mta()?;
    let _mmcss = Mmcss::audio();
    let enumr = wasapi::enumerator()?;
    let device_event = Arc::new(Event::new()?);
    let _notifications = Notifications::register(
        &enumr,
        config.inputs.iter().map(|i| &i.source),
        Arc::clone(&device_event),
    )?;

    let epoch = qpc_now();
    let mixed_inputs = config.inputs.iter().filter(|i| i.mix).count();
    let mut mix = config
        .mix_track
        .map(|track| MixTrack::new(track, mixed_inputs, epoch, config.bitrate_kbps, levels))
        .transpose()?;
    let mut inputs = Vec::with_capacity(config.inputs.len());
    let mut slot = 0;
    for (input, muted) in config.inputs.into_iter().zip(muted) {
        let track = input
            .track
            .map(|t| Track::new(t, config.bitrate_kbps, levels))
            .transpose()?;
        let mix_slot = (input.mix && mix.is_some()).then(|| {
            slot += 1;
            slot - 1
        });
        inputs.push(Input::new(
            input.source,
            track,
            mix_slot,
            epoch,
            muted.clone(),
        ));
    }
    let encoder = inputs
        .iter()
        .find_map(|i| i.down.track.as_ref())
        .or(mix.as_ref().map(|m| &m.track))
        .map(|t| &t.encoder)
        .ok_or_else(|| AudioError::Config("no tracks".into()))?;
    let ready_value = (encoder.format().clone(), encoder.bitrate_kbps());

    for input in &mut inputs {
        input.follow_and_open(&enumr, epoch);
    }
    publish(stats, &inputs, mix.as_ref());
    let _ = ready.send(Ok(ready_value));

    let mut handles: Vec<HANDLE> = Vec::new();
    let device_changed = WAIT_EVENT(WAIT_OBJECT_0.0 + 1);
    let mut last_publish = epoch;
    loop {
        handles.clear();
        handles.extend([stop.handle(), device_event.handle()]);
        handles.extend(inputs.iter().filter_map(Input::event));
        // SAFETY: every handle is a live event owned by this frame, by an
        // open stream (not dropped before the next iteration) or by
        // `AudioCapture`, which joins this thread before closing its own.
        let wait = unsafe { WaitForMultipleObjects(&handles, false, POLL_MS) };
        if wait == WAIT_OBJECT_0 {
            break;
        }
        if wait == device_changed {
            for input in &mut inputs {
                input.on_device_change(&enumr);
            }
        } else if wait != WAIT_TIMEOUT && wait.0 >= WAIT_OBJECT_0.0 + handles.len() as u32 {
            // WAIT_FAILED cannot happen with valid handles; don't spin if it
            // somehow does.
            std::thread::sleep(std::time::Duration::from_millis(POLL_MS.into()));
        }
        let now = qpc_now();
        for input in &mut inputs {
            input.tick(&enumr, now, mix.as_mut().map(|m| &mut m.mixer), &mut *sink)?;
        }
        if let Some(mix) = &mut mix {
            mix.step(now, &mut *sink)?;
        }
        if now - last_publish >= PUBLISH_EVERY {
            last_publish = now;
            publish(stats, &inputs, mix.as_ref());
        }
    }

    let now = qpc_now();
    for input in &mut inputs {
        input.finish(now, mix.as_mut().map(|m| &mut m.mixer), &mut *sink)?;
    }
    if let Some(mix) = &mut mix {
        mix.step(now, &mut *sink)?;
        mix.drain(&mut *sink)?;
    }
    for input in &mut inputs {
        input.stream = None;
    }
    publish(stats, &inputs, mix.as_ref());
    Ok(())
}

fn publish(stats: &Mutex<AudioStats>, inputs: &[Input], mix: Option<&MixTrack>) {
    let per_ms = u64::from(crate::SAMPLE_RATE / 1000);
    let mut s = lock(stats);
    for (out, input) in s.inputs.iter_mut().zip(inputs) {
        let t = input.down.timeline.stats();
        *out = InputStats {
            packets: input.down.track.as_ref().map_or(0, |t| t.out.packets),
            repeated_silent: input.down.track.as_ref().map_or(0, |t| t.repeated),
            silence_ms: t.silence_samples / per_ms,
            dropped_ms: t.dropped_samples / per_ms,
            drift_corrections: t.drift_corrections,
            long_gaps: t.long_gaps,
            restarts: input.restarts,
            device: input
                .stream
                .as_ref()
                .map(|s| s.name.clone().unwrap_or_else(|| s.id.clone())),
            last_error: input.last_error.clone(),
        };
    }
    s.mix_packets = mix.map_or(0, |m| m.track.out.packets);
    s.mix_repeated_silent = mix.map_or(0, |m| m.track.repeated);
}

/// Numbers encoder output and hands it to the sink.
struct Output {
    track: TrackId,
    /// Index of the next packet.
    next_packet: i64,
    packets: u64,
    /// The packet emitted last.
    last: Option<Arc<[u8]>>,
}

impl Output {
    fn emit(&mut self, timeline: &Timeline, data: Arc<[u8]>, sink: &mut dyn FnMut(Packet)) {
        let len = i64::from(SAMPLES_PER_PACKET);
        // Packet i decodes to input samples starting at i * 1024 - delay.
        let first = self.next_packet * len - ENCODER_DELAY_SAMPLES;
        let pts = timeline.time_of(first);
        self.next_packet += 1;
        self.packets += 1;
        self.last = Some(Arc::clone(&data));
        sink(Packet {
            track: self.track,
            segment: 0,
            pts,
            dts: pts,
            duration: timeline.duration_of(first, len),
            keyframe: true,
            data,
        });
    }
}

/// Samples this close to zero count as silence (-84 dBFS, far below
/// hearing). Process loopback carries ±1 LSB of conversion noise while an
/// app is silent; it is encoded as true silence.
const SILENCE_LSB: u16 = 2;

fn is_silent(pcm: &[i16]) -> bool {
    pcm.iter().all(|&s| s.unsigned_abs() <= SILENCE_LSB)
}

/// An AAC frame's window spans the previous frame too, so a packet decodes
/// to silence, and leaves the decoder's overlap state at zero, once the two
/// frames it covers are silent. Four frames leave a margin.
const SETTLED_FRAMES: u64 = 4;

/// Silence, in frames, that must follow a frame before a repeated packet may
/// stand in for it. The packet for a frame is decoded overlapped with the
/// next one, and the encoder looks ahead to pick its window shapes, so the
/// real packets just before audio returns already carry part of it: they
/// must come from the encoder. (Without this, the first frame of every
/// sound after a silence decodes half-faded, an audible click.)
const LOOKAHEAD_FRAMES: u64 = 3;

/// An encoder and its output: one AAC track.
///
/// Most tracks are silent most of the time (voice chat with nobody talking,
/// an idle "other apps" track, a game in a menu). Once the encoder has been
/// fed enough silence that its packets are settled silent packets, the track
/// repeats its last packet instead of encoding, until audio returns, staying
/// [`LOOKAHEAD_FRAMES`] behind the silence received. When audio returns, the
/// encoder is fed the silent frames no copy covers, then the audio. The
/// encoder skips the frames the copies covered, which changes nothing: its
/// state was settled silence before and after them. Silence is only skipped
/// in whole packets, so the encoder stays on its packet grid.
///
/// The MF AAC encoder holds two input frames before emitting their packets
/// (its output still lines up sample for sample, see
/// [`ENCODER_DELAY_SAMPLES`]). Those frames are silent when repetition
/// starts; once copies cover them, the encoder's own packets for them are
/// dropped when they arrive.
struct Track {
    encoder: AacEncoder,
    meter: Meter,
    out: Output,
    /// Samples fed to the encoder.
    fed: u64,
    /// Samples the encoder's packets cover, kept or dropped.
    emitted: u64,
    /// Zero samples fed to the encoder in a row, up to the last one fed.
    silent_run: u64,
    repeating: Option<Repeat>,
    /// Encoder packets still to drop (frames copies already covered).
    drop_next: u64,
    /// Packets repeated instead of encoded.
    repeated: u64,
}

/// A run of repeated silent packets.
struct Repeat {
    packet: Arc<[u8]>,
    /// Samples the encoder held (fed, no packet yet) when the run began.
    held: u64,
    /// Silent samples received since the run began.
    silence: u64,
    /// Copies emitted; the first stands in for the encoder's first held
    /// frame.
    copies: u64,
}

impl Track {
    fn new(track: TrackId, bitrate_kbps: u32, levels: &Levels) -> Result<Self, AudioError> {
        Ok(Self {
            encoder: AacEncoder::new(bitrate_kbps)?,
            meter: Meter::new(track, levels),
            out: Output {
                track,
                next_packet: 0,
                packets: 0,
                last: None,
            },
            fed: 0,
            emitted: 0,
            silent_run: 0,
            repeating: None,
            drop_next: 0,
            repeated: 0,
        })
    }

    /// Encodes `pcm`, or `frames` frames of silence when `pcm` is `None`.
    fn encode(
        &mut self,
        timeline: &Timeline,
        pcm: Option<&[i16]>,
        frames: u64,
        sink: &mut dyn FnMut(Packet),
    ) -> Result<(), AudioError> {
        self.meter.push(timeline, pcm, frames);
        match pcm {
            Some(pcm) if pcm.iter().any(|&s| s != 0) => {
                self.stop_repeating(timeline, sink)?;
                self.feed(timeline, Some(pcm), (pcm.len() / 2) as u64, sink)
            }
            Some(pcm) => self.silence(timeline, (pcm.len() / 2) as u64, sink),
            None => self.silence(timeline, frames, sink),
        }
    }

    fn silence(
        &mut self,
        timeline: &Timeline,
        mut frames: u64,
        sink: &mut dyn FnMut(Packet),
    ) -> Result<(), AudioError> {
        let len = u64::from(SAMPLES_PER_PACKET);
        if self.repeating.is_none() {
            // Feed the encoder up to the first packet boundary at which its
            // last emitted packet is settled: the frames it holds and the
            // settled frames before them are all silent.
            let held = self.fed - self.emitted;
            let needed = held + SETTLED_FRAMES * len;
            let settled_at =
                (self.fed + needed.saturating_sub(self.silent_run)).div_ceil(len) * len;
            let n = (settled_at - self.fed).min(frames);
            self.feed(timeline, None, n, sink)?;
            frames -= n;
            if frames == 0 {
                return Ok(());
            }
            let held = self.fed - self.emitted;
            let settled = self.fed % len == 0
                && held % len == 0
                && self.silent_run >= held + SETTLED_FRAMES * len;
            match (&self.out.last, settled) {
                (Some(packet), true) => {
                    self.repeating = Some(Repeat {
                        packet: Arc::clone(packet),
                        held,
                        silence: 0,
                        copies: 0,
                    });
                }
                // Not settled (the encoder's buffering changed?): just encode.
                _ => return self.feed(timeline, None, frames, sink),
            }
        }
        let Some(run) = &mut self.repeating else {
            return Ok(());
        };
        run.silence += frames;
        // Silence known past the last frame the encoder emitted a packet for.
        let known = run.held + run.silence;
        while (run.copies + 1 + LOOKAHEAD_FRAMES) * len <= known {
            run.copies += 1;
            self.out.emit(timeline, Arc::clone(&run.packet), sink);
            self.repeated += 1;
        }
        Ok(())
    }

    /// Ends a run of repeated packets: the encoder gets the silence no copy
    /// covers, and drops its packets for the frames copies covered.
    fn stop_repeating(
        &mut self,
        timeline: &Timeline,
        sink: &mut dyn FnMut(Packet),
    ) -> Result<(), AudioError> {
        let Some(run) = self.repeating.take() else {
            return Ok(());
        };
        let len = u64::from(SAMPLES_PER_PACKET);
        let covered = run.copies * len;
        self.drop_next += covered.min(run.held) / len;
        let unfed = run.held + run.silence - covered.max(run.held);
        self.feed(timeline, None, unfed, sink)
    }

    /// Hands `frames` samples (`pcm`, or silence) to the encoder.
    fn feed(
        &mut self,
        timeline: &Timeline,
        pcm: Option<&[i16]>,
        frames: u64,
        sink: &mut dyn FnMut(Packet),
    ) -> Result<(), AudioError> {
        if frames == 0 {
            return Ok(());
        }
        let Self {
            encoder,
            out,
            fed,
            emitted,
            silent_run,
            drop_next,
            ..
        } = self;
        let mut emit = |data: &[u8]| {
            *emitted += u64::from(SAMPLES_PER_PACKET);
            if *drop_next > 0 {
                *drop_next -= 1;
            } else {
                out.emit(timeline, Arc::from(data), sink);
            }
        };
        match pcm {
            Some(pcm) => {
                encoder.push_pcm(pcm, &mut emit)?;
                let trailing = pcm.iter().rev().take_while(|&&s| s == 0).count() / 2;
                *silent_run = if trailing == pcm.len() / 2 {
                    *silent_run + frames
                } else {
                    trailing as u64
                };
            }
            None => {
                encoder.push_silence(frames, &mut emit)?;
                *silent_run += frames;
            }
        }
        *fed += frames;
        Ok(())
    }

    fn drain(
        &mut self,
        timeline: &Timeline,
        sink: &mut dyn FnMut(Packet),
    ) -> Result<(), AudioError> {
        // The encoder pads the last partial packet, as it would have.
        self.stop_repeating(timeline, sink)?;
        let Self {
            encoder,
            out,
            drop_next,
            ..
        } = self;
        encoder.drain(&mut |data| {
            if *drop_next > 0 {
                *drop_next -= 1;
            } else {
                out.emit(timeline, Arc::from(data), sink);
            }
        })
    }
}

/// Where an input's samples go once the timeline has placed them.
struct Downstream {
    timeline: Timeline,
    /// Samples passed on so far: the timeline index of the next one.
    fed: i64,
    track: Option<Track>,
    mix_slot: Option<usize>,
    epoch: Hns,
    /// Set from [`AudioCapture::set_muted`].
    muted: Arc<AtomicBool>,
}

impl Downstream {
    /// Passes `frames` samples (`pcm`, or silence when `None`) on to the
    /// input's own track and to the mix.
    fn feed(
        &mut self,
        pcm: Option<&[i16]>,
        frames: u64,
        mixer: Option<&mut Mixer>,
        sink: &mut dyn FnMut(Packet),
    ) -> Result<(), AudioError> {
        if frames == 0 {
            return Ok(());
        }
        // Near-silence is passed on as true silence: the track can repeat a
        // silent packet and the mix stays silent. A muted input is silence.
        let muted = self.muted.load(Ordering::Relaxed);
        let pcm = pcm.filter(|pcm| !muted && !is_silent(pcm));
        let first = self.fed;
        let end = first + frames as i64;
        self.fed = end;
        if let Some(track) = &mut self.track {
            track.encode(&self.timeline, pcm, frames, sink)?;
        }
        if let (Some(slot), Some(mixer)) = (self.mix_slot, mixer) {
            // Hand over evenly spaced pieces: the mapping only jumps inside
            // the silence of a long gap.
            let mut index = first;
            while index < end {
                let piece_end = self
                    .timeline
                    .mapping_break_after(index)
                    .map_or(end, |b| b.min(end));
                let grid = grid_index(self.timeline.time_of(index), self.epoch);
                let n = (piece_end - index) as usize;
                match pcm {
                    Some(pcm) => {
                        let from = (index - first) as usize * 2;
                        mixer.push(slot, grid, &pcm[from..from + n * 2]);
                    }
                    None => mixer.push_silence(slot, grid, n),
                }
                index = piece_end;
            }
        }
        Ok(())
    }
}

/// One captured source.
struct Input {
    source: AudioSource,
    /// The app a process loopback input follows, while it runs.
    followed: Option<Followed>,
    stream: Option<Stream>,
    /// QPC time of the last open attempt while no stream is running.
    last_attempt: Hns,
    opened_once: bool,
    restarts: u32,
    last_error: Option<String>,
    down: Downstream,
}

impl Input {
    fn new(
        source: AudioSource,
        track: Option<Track>,
        mix_slot: Option<usize>,
        epoch: Hns,
        muted: Arc<AtomicBool>,
    ) -> Self {
        Self {
            source,
            followed: None,
            stream: None,
            last_attempt: i64::MIN / 2,
            opened_once: false,
            restarts: 0,
            last_error: None,
            down: Downstream {
                timeline: Timeline::new(epoch, TimelineConfig::default()),
                fed: 0,
                track,
                mix_slot,
                epoch,
                muted,
            },
        }
    }

    fn event(&self) -> Option<HANDLE> {
        self.stream.as_ref()?.event.as_ref().map(Event::handle)
    }

    /// One wake-up: read what the device delivered, fill silence for what it
    /// did not, follow the app / retry a missing device now and then.
    fn tick(
        &mut self,
        enumr: &IMMDeviceEnumerator,
        now: Hns,
        mut mixer: Option<&mut Mixer>,
        sink: &mut dyn FnMut(Packet),
    ) -> Result<(), AudioError> {
        self.read_stream(enumr, mixer.as_deref_mut(), sink)?;
        self.follow_and_open(enumr, now);
        let silence = self.down.timeline.on_idle(now);
        self.down.feed(None, silence, mixer, sink)
    }

    fn finish(
        &mut self,
        now: Hns,
        mut mixer: Option<&mut Mixer>,
        sink: &mut dyn FnMut(Packet),
    ) -> Result<(), AudioError> {
        if let Some(stream) = &mut self.stream {
            // Errors here only lose the last few milliseconds.
            let down = &mut self.down;
            let _ = stream.read(|chunk| place_chunk(down, chunk, mixer.as_deref_mut(), sink));
        }
        let silence = self.down.timeline.on_finish(now);
        self.down.feed(None, silence, mixer, sink)?;
        if let Some(track) = &mut self.down.track {
            track.drain(&self.down.timeline, sink)?;
        }
        Ok(())
    }

    /// Drains the stream. Device loss is handled here (the stream is dropped
    /// and reopened); only encoder failures are returned.
    fn read_stream(
        &mut self,
        enumr: &IMMDeviceEnumerator,
        mut mixer: Option<&mut Mixer>,
        sink: &mut dyn FnMut(Packet),
    ) -> Result<(), AudioError> {
        let Some(stream) = &mut self.stream else {
            return Ok(());
        };
        let down = &mut self.down;
        let mut encode_error = false;
        let result = stream.read(|chunk| {
            let r = place_chunk(down, chunk, mixer.as_deref_mut(), sink);
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
                self.last_error = Some(e.to_string());
                self.close_stream();
                self.try_open(enumr);
                Ok(())
            }
        }
    }

    fn close_stream(&mut self) {
        self.stream = None;
        self.down.timeline.mark_discontinuity();
    }

    fn on_device_change(&mut self, enumr: &IMMDeviceEnumerator) {
        let switch = match &self.stream {
            None => true,
            Some(stream) => {
                self.source.follows_default()
                    && wasapi::current_device_id(enumr, &self.source)
                        .is_some_and(|id| id != stream.id)
            }
        };
        if switch {
            self.close_stream();
            self.try_open(enumr);
        }
    }

    /// For process loopback: notices the app exiting and finds it (again).
    /// Retries opening a missing stream every [`RETRY`].
    fn follow_and_open(&mut self, enumr: &IMMDeviceEnumerator, now: Hns) {
        let due = now - self.last_attempt >= RETRY;
        if matches!(self.source, AudioSource::Process { .. }) {
            if self.followed.as_ref().is_some_and(|f| !f.alive()) {
                // Its stream would only carry silence from now on, and a
                // restarted app is a new process.
                self.followed = None;
                self.close_stream();
            }
            if self.followed.is_none() {
                if due {
                    self.last_attempt = now;
                    if let AudioSource::Process { target, .. } = &self.source {
                        self.followed = process::resolve(target);
                    }
                    if self.followed.is_some() {
                        self.try_open(enumr);
                    }
                }
                return;
            }
        }
        if self.stream.is_none() && due {
            self.try_open(enumr);
        }
    }

    fn try_open(&mut self, enumr: &IMMDeviceEnumerator) {
        self.last_attempt = qpc_now();
        if matches!(self.source, AudioSource::Process { .. }) && self.followed.is_none() {
            return;
        }
        match Stream::open(enumr, &self.source, self.followed.as_ref()) {
            Ok(stream) => {
                if self.opened_once {
                    self.restarts += 1;
                }
                self.opened_once = true;
                self.stream = Some(stream);
                self.down.timeline.mark_discontinuity();
            }
            Err(e) => self.last_error = Some(e.to_string()),
        }
    }
}

/// Places one captured chunk on the timeline and feeds it on.
fn place_chunk(
    down: &mut Downstream,
    chunk: Chunk<'_>,
    mut mixer: Option<&mut Mixer>,
    sink: &mut dyn FnMut(Packet),
) -> Result<(), AudioError> {
    if chunk.discontinuity {
        down.timeline.mark_discontinuity();
    }
    let place = down.timeline.on_data(chunk.time, chunk.frames, qpc_now());
    down.feed(None, place.silence_before, mixer.as_deref_mut(), sink)?;
    let kept = u64::from(chunk.frames - place.skip);
    match chunk.pcm {
        Some(pcm) => down.feed(Some(&pcm[place.skip as usize * 2..]), kept, mixer, sink),
        None => down.feed(None, kept, mixer, sink),
    }
}

/// The mixed track: mixer, output timeline and encoder.
struct MixTrack {
    mixer: Mixer,
    timeline: Timeline,
    track: Track,
    epoch: Hns,
}

impl MixTrack {
    fn new(
        track: TrackId,
        inputs: usize,
        epoch: Hns,
        bitrate_kbps: u32,
        levels: &Levels,
    ) -> Result<Self, AudioError> {
        let cfg = TimelineConfig::default();
        Ok(Self {
            // Inputs only leave gaps longer than the timeline fill limit, so
            // skip exactly those.
            mixer: Mixer::new(inputs, hns_to_samples(cfg.max_fill) as i64),
            timeline: Timeline::new(epoch, cfg),
            track: Track::new(track, bitrate_kbps, levels)?,
            epoch,
        })
    }

    /// Encodes everything the mixer can produce.
    fn step(&mut self, now: Hns, sink: &mut dyn FnMut(Packet)) -> Result<(), AudioError> {
        for block in self.mixer.mix(MIX_BLOCK) {
            match block {
                // The output jumps too: the next block is placed by its time
                // (a long gap on the mixed track's timeline).
                Mixed::Skip { .. } => self.timeline.mark_discontinuity(),
                Mixed::Samples { start, pcm } => {
                    let frames = (pcm.len() / 2) as u32;
                    let t = self.epoch + samples_to_hns(start);
                    // Exact timestamps: never drift, only anchor.
                    let place = self.timeline.on_data(Some(t), frames, now);
                    self.track
                        .encode(&self.timeline, None, place.silence_before, sink)?;
                    let kept = &pcm[place.skip as usize * 2..];
                    self.track
                        .encode(&self.timeline, Some(kept), (kept.len() / 2) as u64, sink)?;
                }
            }
        }
        Ok(())
    }

    fn drain(&mut self, sink: &mut dyn FnMut(Packet)) -> Result<(), AudioError> {
        self.track.drain(&self.timeline, sink)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Once;

    use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_LITE, MFStartup};

    use super::*;
    use crate::aac;

    fn decode(packets: &[Packet], name: &str) -> Vec<i16> {
        let mut adts = Vec::new();
        for p in packets {
            adts.extend_from_slice(&aac::adts_header(48_000, 2, p.len()).unwrap());
            adts.extend_from_slice(&p.data);
        }
        let path = std::env::temp_dir().join(format!("rec-audio-{name}.aac"));
        std::fs::write(&path, &adts).unwrap();
        let out = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&path)
            .args(["-f", "s16le", "-acodec", "pcm_s16le", "-"])
            .output()
            .expect("run ffmpeg");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect()
    }

    /// Repeating silent packets must decode exactly like encoding the
    /// silence: same length, no clicks where audio starts or stops. Encodes
    /// bursts of a tone with sharp edges, separated by silence and by ±1 LSB
    /// noise, once through `Track` and once straight through the encoder,
    /// and compares the decoded signals.
    ///
    /// `cargo test -p rec-audio --lib -- --ignored --nocapture silent_packet`
    #[test]
    #[ignore = "needs ffmpeg on PATH"]
    fn silent_packet_repeat_is_transparent() {
        static STARTUP: Once = Once::new();
        // SAFETY: process-wide MF startup, done once and never shut down.
        STARTUP.call_once(|| unsafe {
            let _ = MFStartup(MF_VERSION, MFSTARTUP_LITE);
        });
        let _com = ComGuard::mta().unwrap();
        let Ok(mut track) = Track::new(1, 160, &Levels::default()) else {
            eprintln!("skipping: Media Foundation AAC encoder unavailable");
            return;
        };
        let mut plain = AacEncoder::new(160).unwrap();

        let tone = |n: usize, phase: usize| -> Vec<i16> {
            (0..n)
                .flat_map(|i| {
                    let t = (i + phase) as f64 / 48_000.0;
                    let v = ((t * 997.0 * std::f64::consts::TAU).sin() * 9000.0) as i16;
                    [v, v / 2]
                })
                .collect()
        };
        let mut signal = Vec::new();
        signal.extend(tone(24_000, 0));
        signal.extend(vec![0i16; 48_000 * 2]);
        signal.extend(tone(12_345, 7));
        signal.extend((0..30_000 * 2).map(|i| [1i16, -1, 0, 2][i % 4]));
        signal.extend(tone(20_000, 3));
        signal.extend(vec![0i16; 70_001 * 2]);

        let timeline = Timeline::new(0, TimelineConfig::default());
        let mut repeated_packets = Vec::new();
        let mut plain_packets = Vec::new();
        for chunk in signal.chunks(960) {
            // As `Downstream::feed` passes it on.
            let pcm = Some(chunk).filter(|pcm| !is_silent(pcm));
            track
                .encode(&timeline, pcm, (chunk.len() / 2) as u64, &mut |p| {
                    repeated_packets.push(p)
                })
                .unwrap();
            // The reference sees the same silence the track does.
            let reference: Vec<i16> = if is_silent(chunk) {
                vec![0; chunk.len()]
            } else {
                chunk.to_vec()
            };
            plain
                .push_pcm(&reference, &mut |d| plain_packets.push(d.to_vec()))
                .unwrap();
        }
        track
            .drain(&timeline, &mut |p| repeated_packets.push(p))
            .unwrap();
        plain
            .drain(&mut |d| plain_packets.push(d.to_vec()))
            .unwrap();

        assert!(track.repeated > 50, "only {} repeated", track.repeated);
        assert_eq!(repeated_packets.len(), plain_packets.len());
        let plain_packets: Vec<Packet> = plain_packets
            .into_iter()
            .map(|d| Packet {
                track: 1,
                segment: 0,
                pts: 0,
                dts: 0,
                duration: 0,
                keyframe: true,
                data: Arc::from(d),
            })
            .collect();
        let a = decode(&repeated_packets, "repeat");
        let b = decode(&plain_packets, "plain");
        assert_eq!(a.len(), b.len());
        let (mut err, mut sig, mut worst) = (0f64, 0f64, 0i32);
        for (&x, &y) in a.iter().zip(&b) {
            let d = i32::from(x) - i32::from(y);
            err += f64::from(d * d);
            sig += f64::from(y) * f64::from(y);
            worst = worst.max(d.abs());
        }
        let snr = 10.0 * (sig / err.max(1.0)).log10();
        println!(
            "{} packets, {} repeated, SNR vs plain encode {snr:.1} dB, worst sample difference {worst}",
            repeated_packets.len(),
            track.repeated
        );
        // A click is a difference of thousands; codec noise a few dozen.
        assert!(snr > 50.0 && worst < 500, "SNR {snr:.1} dB, worst {worst}");
    }
}
