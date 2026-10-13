//! One recording session: the video and audio pipelines for one target, all
//! feeding a shared replay buffer of encoded packets. The video pipeline is
//! restarted in place when it fails or the source changes (see
//! [`crate::recover`]); each run is one `segment`.

use std::sync::{Arc, Mutex};

use rec_audio::{AudioCapture, AudioCaptureConfig, AudioInput, AudioSource, ProcessTarget};
use rec_core::config::{
    AudioConfig, Codec, Config, EncoderPreference, HdrMode, Quality, VideoConfig,
};
use rec_core::media::{TrackInfo, TrackKind, VIDEO_TRACK, VideoFormat};
use rec_core::replay::{Limits, Packet, ReplayBuffer, TrackId};
use rec_core::time::{Hns, as_secs_f64, qpc_now, secs};
use rec_video::geometry::output_size;
use rec_video::{CaptureSource, CodecPreference, EncoderBackend, VideoCapture, VideoCaptureConfig};
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
    MonitorFromWindow,
};
use windows::Win32::UI::Shell::{QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetForegroundWindow, GetWindowDisplayAffinity, IsIconic, WDA_NONE,
};
use windows::core::BOOL;

use crate::detect;
use crate::log::{info, warning};
use crate::recover::{DisplayChange, Recovery};
use crate::watch::{Observation, Problem, Watcher};
use crate::win::Hwnd;

/// What a session records.
#[derive(Clone, Debug)]
pub struct Target {
    /// The game window; `Hwnd(0)` for desktop recording without a game.
    pub hwnd: Hwnd,
    /// Friendly name used for folders and file names.
    pub game: String,
    pub exe_name: String,
    /// The game's process id; 0 for desktop recording.
    pub pid: u32,
    /// Record the whole display instead of just the window.
    pub display: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    Ok,
    /// The video pipeline stopped and will be restarted; audio and the
    /// buffer carry on.
    Restarting,
    SourceClosed,
    /// The video pipeline could not be brought back.
    Failed(String),
}

#[derive(Debug)]
pub struct Session {
    pub target: Target,
    buffer: Arc<Mutex<ReplayBuffer>>,
    /// Video track first; its format is the newest segment's.
    tracks: Vec<TrackInfo>,
    description: String,
    // Video is declared before audio so it stops first when dropped.
    /// `None` while waiting to restart after a failure.
    video: Option<VideoCapture>,
    /// Format of every segment started so far, oldest first.
    segments: Vec<(u32, VideoFormat)>,
    source: CaptureSource,
    settings: VideoConfig,
    /// Buffer length in seconds and memory cap in MB, for the bitrate check.
    buffer_seconds: u32,
    max_memory_mb: u32,
    post_roll_seconds: u32,
    /// The video bitrate in use.
    bitrate_kbps: u32,
    recovery: Recovery,
    audio: Option<AudioCapture>,
    /// Per audio input: its name and the setting it records alone.
    audio_inputs: Vec<(&'static str, Option<AudioSwitch>)>,
    watcher: Watcher,
    problem: Option<Problem>,
}

/// Seconds kept beyond the requested clip length: room for keyframe
/// alignment, post-roll and the save grace period.
const BUFFER_SLACK_SECONDS: u32 = 3;

fn sink(buffer: &Arc<Mutex<ReplayBuffer>>) -> Box<dyn FnMut(Packet) + Send> {
    let buffer = buffer.clone();
    Box::new(move |packet| {
        if let Ok(mut buffer) = buffer.lock() {
            buffer.push(packet);
        }
    })
}

/// Default video bitrate for a quality preset, scaled from a 1080p60 HEVC
/// baseline by pixel count and frame rate (sub-linearly: encoders get more
/// efficient as resolution and frame rate rise).
pub fn default_bitrate_kbps(
    quality: Quality,
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
) -> u32 {
    let base = match quality {
        Quality::Performance => 10_000.0,
        Quality::Balanced => 18_000.0,
        Quality::Quality => 30_000.0,
    };
    let pixels = f64::from(width.max(1) * height.max(1)) / (1920.0 * 1080.0);
    let rate = f64::from(fps.max(1)) / 60.0;
    let codec = match codec {
        Codec::H264 => 1.5,
        Codec::Auto | Codec::Hevc | Codec::Av1 => 1.0,
    };
    (base * pixels.powf(0.75) * rate.powf(0.6) * codec).clamp(3_000.0, 150_000.0) as u32
}

/// An audio source setting that can be switched while recording (the quick
/// panel's audio rows).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioSwitch {
    Game,
    VoiceChat,
    Microphone,
    OtherApps,
}

impl AudioSwitch {
    pub const ALL: [Self; 4] = [
        Self::Game,
        Self::VoiceChat,
        Self::Microphone,
        Self::OtherApps,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Game => "Game",
            Self::VoiceChat => "Voice chat",
            Self::Microphone => "Microphone",
            Self::OtherApps => "Other apps",
        }
    }

    pub fn get(self, a: &AudioConfig) -> bool {
        match self {
            Self::Game => a.game,
            Self::VoiceChat => a.voice_chat,
            Self::Microphone => a.microphone,
            Self::OtherApps => a.other_apps,
        }
    }

    pub fn set(self, a: &mut AudioConfig, on: bool) {
        *match self {
            Self::Game => &mut a.game,
            Self::VoiceChat => &mut a.voice_chat,
            Self::Microphone => &mut a.microphone,
            Self::OtherApps => &mut a.other_apps,
        } = on;
    }

    /// Its key in the config file.
    pub fn key(self) -> &'static str {
        match self {
            Self::Game => "audio.game",
            Self::VoiceChat => "audio.voice_chat",
            Self::Microphone => "audio.microphone",
            Self::OtherApps => "audio.other_apps",
        }
    }
}

/// The audio inputs and tracks a session records, with each track's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioPlan {
    pub config: AudioCaptureConfig,
    /// `(track, name)` in track order, mixed track first.
    pub names: Vec<(TrackId, &'static str)>,
    /// Per input (in `config.inputs` order): its name, and the setting it
    /// records alone (`None` for system loopback standing in for several).
    pub inputs: Vec<(&'static str, Option<AudioSwitch>)>,
}

/// Turns the audio settings into inputs and tracks (video is track 0).
///
/// With per-app capture (`game_pid` known and process loopback supported):
/// the game, voice chat apps and "other apps" (everything except the game)
/// each get a track. Without it, one system-loopback track stands in for all
/// three. The microphone is always its own track. The mixed track sums what
/// a viewer should hear: game, voice chat (or "other apps", which already
/// contains voice chat) and the microphone; it is only written when it would
/// differ from a single track.
pub fn plan_audio(a: &AudioConfig, game_pid: Option<u32>, per_app: bool) -> Option<AudioPlan> {
    let process = |target: ProcessTarget, exclude: bool| AudioSource::Process { target, exclude };
    // (source, name, in the mix, the setting it records alone)
    let mut inputs: Vec<(AudioSource, &'static str, bool, Option<AudioSwitch>)> = Vec::new();
    let game = Some(AudioSwitch::Game);
    match game_pid.filter(|_| per_app) {
        Some(pid) => {
            if a.game {
                inputs.push((process(ProcessTarget::Pid(pid), false), "Game", true, game));
            }
            if a.voice_chat && !a.voice_chat_apps.is_empty() {
                let apps = ProcessTarget::Executables(a.voice_chat_apps.clone());
                inputs.push((
                    process(apps, false),
                    "Voice chat",
                    !a.other_apps,
                    Some(AudioSwitch::VoiceChat),
                ));
            }
            if a.other_apps {
                inputs.push((
                    process(ProcessTarget::Pid(pid), true),
                    "Other apps",
                    true,
                    Some(AudioSwitch::OtherApps),
                ));
            }
        }
        None => {
            if a.game || a.voice_chat || a.other_apps {
                let name = if game_pid.is_some() {
                    "Game & system audio"
                } else {
                    "System audio"
                };
                inputs.push((AudioSource::SystemLoopback, name, true, None));
            }
        }
    }
    if a.microphone {
        let mic = AudioSource::Microphone {
            device_id: a.microphone_device.clone(),
        };
        inputs.push((mic, "Microphone", true, Some(AudioSwitch::Microphone)));
    }
    if inputs.is_empty() {
        return None;
    }

    let mixed = inputs.iter().filter(|(_, _, mix, _)| *mix).count();
    let switches = inputs.iter().map(|&(_, name, _, s)| (name, s)).collect();
    let mix = a.mixed_track && mixed >= 2;
    let mut names = Vec::new();
    let mut next: TrackId = VIDEO_TRACK + 1;
    let mix_track = mix.then(|| {
        names.push((next, "Mixed"));
        next += 1;
        next - 1
    });
    let inputs = inputs
        .into_iter()
        .map(|(source, name, in_mix, _)| {
            names.push((next, name));
            next += 1;
            AudioInput {
                source,
                track: Some(next - 1),
                mix: mix && in_mix,
            }
        })
        .collect();
    Some(AudioPlan {
        config: AudioCaptureConfig {
            inputs,
            mix_track,
            bitrate_kbps: a.bitrate_kbps,
        },
        names,
        inputs: switches,
    })
}

fn monitor_by_index(index: u32) -> Option<HMONITOR> {
    unsafe extern "system" fn collect(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        // SAFETY: `data` is the `&mut Vec` passed below, alive for the call.
        let monitors = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
        monitors.push(monitor);
        true.into()
    }
    let mut monitors: Vec<HMONITOR> = Vec::new();
    // SAFETY: the callback only runs during this call.
    let _ = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(&mut monitors as *mut _ as isize),
        )
    };
    monitors.get(index as usize).copied()
}

fn source_size(source: CaptureSource, hwnd: Hwnd) -> (u32, u32) {
    // SAFETY: plain queries on handles that may be stale; failures fall back
    // to 1080p for the bitrate estimate only.
    unsafe {
        let rect = match source {
            CaptureSource::Window(_) => {
                let mut rect = RECT::default();
                GetClientRect(hwnd.get(), &mut rect).ok().map(|()| rect)
            }
            CaptureSource::Monitor(m) => {
                let mut mi = MONITORINFO {
                    cbSize: size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                GetMonitorInfoW(HMONITOR(m as *mut _), &mut mi)
                    .as_bool()
                    .then_some(mi.rcMonitor)
            }
            CaptureSource::PrimaryMonitor => None,
        };
        rect.map(|r| {
            (
                (r.right - r.left).max(1) as u32,
                (r.bottom - r.top).max(1) as u32,
            )
        })
        .unwrap_or((1920, 1080))
    }
}

/// Starts a video pipeline for `source` as `segment`, at the bitrate for the
/// source's current size. Returns it with its format and a short
/// description.
fn open_video(
    target: &Target,
    source: CaptureSource,
    v: &VideoConfig,
    buffer: &Arc<Mutex<ReplayBuffer>>,
    segment: u32,
    max_memory_mb: u32,
    buffer_seconds: u32,
) -> Result<(VideoCapture, VideoFormat, u32, String), String> {
    let codec = match v.codec {
        Codec::Auto => CodecPreference::Auto,
        Codec::H264 => CodecPreference::H264,
        Codec::Hevc => CodecPreference::Hevc,
        Codec::Av1 => CodecPreference::Av1,
    };
    // NVIDIA and Intel have no native backend yet; their Media Foundation
    // encoders are the same hardware.
    let encoder = match v.encoder {
        EncoderPreference::Auto => EncoderBackend::Auto,
        EncoderPreference::Amd => EncoderBackend::Amf,
        EncoderPreference::MediaFoundation
        | EncoderPreference::Nvidia
        | EncoderPreference::Intel
        | EncoderPreference::Software => EncoderBackend::MediaFoundation,
    };
    let hdr = match v.hdr {
        HdrMode::Auto => rec_video::HdrMode::Auto,
        HdrMode::Tonemap => rec_video::HdrMode::Tonemap,
        HdrMode::Off => rec_video::HdrMode::Off,
    };
    let (src_w, src_h) = source_size(source, target.hwnd);
    let (out_w, out_h) = output_size(src_w, src_h, v.max_height);
    let bitrate_kbps = v
        .bitrate_kbps
        .unwrap_or_else(|| default_bitrate_kbps(v.quality, v.codec, out_w, out_h, v.fps));

    let needed = u64::from(bitrate_kbps) * 1000 / 8 * u64::from(buffer_seconds);
    if needed > u64::from(max_memory_mb) * 1024 * 1024 {
        warning!(
            "{buffer_seconds} s at {bitrate_kbps} kbit/s needs ~{} MB but the buffer is capped at {max_memory_mb} MB; clips may be shorter",
            needed / (1024 * 1024),
        );
    }

    let config = VideoCaptureConfig {
        source,
        fps: v.fps,
        max_height: v.max_height,
        codec,
        encoder,
        hdr,
        assume_hdr_display: assume_hdr_display(),
        bitrate_kbps,
        cursor: v.cursor,
        track: VIDEO_TRACK,
        segment,
    };
    let video = VideoCapture::start(config, sink(buffer)).map_err(|e| e.to_string())?;
    for note in &video.info().notes {
        warning!("video: {note}");
    }
    let format = video.info().format.clone();
    let description = describe(&video, bitrate_kbps);
    Ok((video, format, bitrate_kbps, description))
}

/// E.g. "HEVC 2560x1440@60 HDR10 18 Mbit/s · AMD Radeon RX 7800 XT".
fn describe(video: &VideoCapture, bitrate_kbps: u32) -> String {
    let info = video.info();
    let f = &info.format;
    format!(
        "{} {}x{}@{}{} {} Mbit/s · {}",
        f.codec.name(),
        f.width,
        f.height,
        f.fps,
        if f.color.is_hdr() { " HDR10" } else { "" },
        crate::panel::mbit(bitrate_kbps),
        info.adapter_name
    )
}

/// Debug builds only: `REC_DEBUG_ASSUME_HDR=1` records as if the display
/// were in HDR mode (FP16 capture, HDR10 or tone-mapped clips), to test the
/// HDR path without switching the display.
fn assume_hdr_display() -> bool {
    cfg!(debug_assertions) && std::env::var_os("REC_DEBUG_ASSUME_HDR").is_some()
}

impl Session {
    pub fn start(config: &Config, target: Target) -> Result<Self, String> {
        let replay = &config.replay;
        let seconds = replay.seconds + replay.post_roll_seconds + BUFFER_SLACK_SECONDS;
        let max_bytes = replay.max_memory_mb as usize * 1024 * 1024;
        let buffer = Arc::new(Mutex::new(ReplayBuffer::new(
            VIDEO_TRACK,
            Limits {
                duration: secs(i64::from(seconds)),
                max_bytes,
            },
        )));

        // ---- video ----
        let v = &config.video;
        let source = if target.display {
            let monitor = v
                .display
                .and_then(monitor_by_index)
                // SAFETY: MonitorFromWindow accepts stale or null handles.
                .or_else(|| {
                    (target.hwnd.0 != 0).then(|| unsafe {
                        MonitorFromWindow(target.hwnd.get(), MONITOR_DEFAULTTOPRIMARY)
                    })
                });
            monitor.map_or(CaptureSource::PrimaryMonitor, |m| {
                CaptureSource::Monitor(m.0 as isize)
            })
        } else {
            CaptureSource::Window(target.hwnd.0)
        };
        match v.encoder {
            EncoderPreference::Nvidia | EncoderPreference::Intel => info!(
                "no native {} encoder backend yet; using its Media Foundation encoder",
                if v.encoder == EncoderPreference::Nvidia {
                    "NVIDIA"
                } else {
                    "Intel"
                }
            ),
            EncoderPreference::Software => {
                warning!("software encoding is not supported yet; using the hardware encoder")
            }
            _ => {}
        }
        let (video, format, bitrate_kbps, description) = open_video(
            &target,
            source,
            v,
            &buffer,
            0,
            replay.max_memory_mb,
            seconds,
        )?;
        let info = video.info();
        let encoder = (info.encoder_name.clone(), info.rate_control.clone());

        let mut tracks = vec![TrackInfo {
            id: VIDEO_TRACK,
            name: "Video".into(),
            kind: TrackKind::Video(format.clone()),
        }];

        // ---- audio ----
        let per_app = rec_audio::process_loopback_supported();
        let game_pid = (target.pid != 0).then_some(target.pid);
        let mut audio = None;
        let mut audio_inputs = Vec::new();
        if let Some(plan) = plan_audio(&config.audio, game_pid, per_app) {
            match AudioCapture::start(plan.config, sink(&buffer)) {
                Ok(capture) => {
                    for &(id, name) in &plan.names {
                        tracks.push(TrackInfo {
                            id,
                            name: name.into(),
                            kind: TrackKind::Audio(capture.format().clone()),
                        });
                    }
                    audio = Some(capture);
                    audio_inputs = plan.inputs;
                }
                Err(e) => warning!("audio unavailable, recording without it: {e}"),
            }
        }
        let audio_tracks = tracks.len() - 1;

        info!(
            "session: {description}, encoder {}, rate control {}, {} audio track(s), buffer {seconds} s / {} MB",
            encoder.0, encoder.1, audio_tracks, replay.max_memory_mb
        );
        let now = qpc_now();
        Ok(Self {
            target,
            buffer,
            tracks,
            description,
            video: Some(video),
            segments: vec![(0, format)],
            source,
            settings: config.video.clone(),
            buffer_seconds: seconds,
            max_memory_mb: replay.max_memory_mb,
            post_roll_seconds: replay.post_roll_seconds,
            bitrate_kbps,
            recovery: Recovery::new(now),
            audio,
            audio_inputs,
            watcher: Watcher::new(now),
            problem: None,
        })
    }

    /// Checks the video pipeline and restarts it when it failed, or the
    /// source changed size or moved to another GPU (see [`crate::recover`]).
    /// Call regularly; restarts happen on the calling thread.
    pub fn supervise(&mut self) -> Health {
        let now = qpc_now();
        let Some(video) = &self.video else {
            if !self.recovery.retry_due(now) {
                return Health::Restarting;
            }
            return self.restart_video(now, None);
        };
        let stats = video.stats();
        if stats.source_closed {
            return Health::SourceClosed;
        }
        if !stats.running {
            let reason = stats
                .last_error
                .unwrap_or_else(|| "video capture stopped".into());
            warning!("{}: video stopped: {reason}", self.target.game);
            self.video = None;
            let retry = self.recovery.crashed(now);
            return self.retry_later(now, reason, retry);
        }
        let info = video.info();
        let encoded = (info.picture_width, info.picture_height);
        let wanted = stats
            .source_size
            .map(|(w, h)| output_size(w, h, self.settings.max_height));
        let display = rec_video::display_for(self.source).ok().and_then(|d| {
            if d.adapter_luid != info.adapter_luid {
                return Some(DisplayChange::Adapter);
            }
            // With HDR off rec captures SDR either way, and an assumed HDR
            // display never changes.
            let on = d.color?.hdr;
            (self.settings.hdr != HdrMode::Off && !assume_hdr_display() && on != info.display_hdr)
                .then_some(DisplayChange::Hdr { on })
        });
        match self.recovery.check(now, encoded, wanted, display) {
            Some(change) => self.restart_video(now, Some(&change.to_string())),
            None => Health::Ok,
        }
    }

    /// Stops the video pipeline, if one runs, and starts the next segment.
    fn restart_video(&mut self, now: Hns, why: Option<&str>) -> Health {
        if let Some(why) = why {
            info!("{}: restarting video: {why}", self.target.game);
        }
        // The old pipeline lets go of its encoder first: GPUs limit how many
        // encode sessions run at once.
        if let Some(video) = self.video.take() {
            video.stop();
        }
        let segment = self.segments.last().map_or(0, |&(s, _)| s + 1);
        let opened = open_video(
            &self.target,
            self.source,
            &self.settings,
            &self.buffer,
            segment,
            self.max_memory_mb,
            self.buffer_seconds,
        );
        match opened {
            Ok((video, format, bitrate_kbps, description)) => {
                info!(
                    "{}: video segment {segment}: {description}",
                    self.target.game
                );
                let started = qpc_now();
                self.recovery
                    .started(started, (format.width, format.height));
                self.tracks[0].kind = TrackKind::Video(format.clone());
                self.segments.push((segment, format));
                self.description = description;
                self.bitrate_kbps = bitrate_kbps;
                self.video = Some(video);
                // The new pipeline's frames are judged afresh.
                self.watcher = Watcher::new(started);
                self.problem = None;
                Health::Ok
            }
            Err(e) => {
                warning!("{}: video did not start: {e}", self.target.game);
                let retry = self.recovery.start_failed(now);
                self.retry_later(now, e, retry)
            }
        }
    }

    fn retry_later(&self, now: Hns, reason: String, at: Option<Hns>) -> Health {
        match at {
            Some(at) => {
                info!(
                    "{}: retrying video in {:.0} s",
                    self.target.game,
                    as_secs_f64(at - now)
                );
                Health::Restarting
            }
            None => Health::Failed(reason),
        }
    }

    /// Checks that the game is really being captured (see [`crate::watch`]).
    /// Returns the new state when it changed.
    pub fn watch(&mut self) -> Option<Option<Problem>> {
        let hwnd = self.target.hwnd;
        if hwnd.0 == 0 {
            // Desktop recording: a still desktop sends no frames, rightly.
            return None;
        }
        let stats = self.video.as_ref()?.stats();
        let mut affinity = WDA_NONE.0;
        // SAFETY: plain queries that accept stale handles; `affinity` is a
        // valid out-pointer.
        let (in_front, blocks_capture) = unsafe {
            let in_front = GetForegroundWindow() == hwnd.get() && !IsIconic(hwnd.get()).as_bool();
            let blocks = GetWindowDisplayAffinity(hwnd.get(), &mut affinity).is_ok()
                && affinity != WDA_NONE.0;
            (in_front, blocks)
        };
        let covers_monitor = detect::window_info(hwnd.get()).is_some_and(|i| i.covers_monitor);
        let obs = Observation {
            now: qpc_now(),
            last_frame: stats.last_frame_time,
            black_since: stats.black_since,
            in_front,
            covers_monitor,
            blocks_capture,
            // SAFETY: plain query.
            d3d_fullscreen: unsafe { SHQueryUserNotificationState() }
                .is_ok_and(|s| s == QUNS_RUNNING_D3D_FULL_SCREEN),
        };
        let problem = self.watcher.check(&obs);
        if problem == self.problem {
            return None;
        }
        match problem {
            Some(p) => warning!("{}: {}", self.target.game, p.short()),
            None => info!("{}: capture looks fine again", self.target.game),
        }
        self.problem = problem;
        Some(problem)
    }

    /// What is wrong with the recording right now, in a few words: the
    /// video is restarting, or the watcher sees a capture problem.
    pub fn warning(&self) -> Option<&'static str> {
        if self.video.is_none() {
            Some("video stopped; restarting it")
        } else {
            self.problem.map(Problem::short)
        }
    }

    /// The watcher sees true exclusive fullscreen: nothing can be drawn
    /// over the game, and taking focus from it would minimize it.
    pub fn exclusive(&self) -> bool {
        self.problem == Some(Problem::Exclusive)
    }

    /// Changes the clip length in place. The buffer keeps what it holds:
    /// a longer one fills up from now, a shorter one drops its oldest part.
    pub fn set_seconds(&mut self, seconds: u32) {
        self.buffer_seconds = seconds + self.post_roll_seconds + BUFFER_SLACK_SECONDS;
        if let Ok(mut buffer) = self.buffer.lock() {
            let limits = Limits {
                duration: secs(i64::from(self.buffer_seconds)),
                ..buffer.limits()
            };
            buffer.set_limits(limits);
        }
    }

    /// Applies new video settings. A bitrate change (quality, or the
    /// bitrate override) is made in the running encoder, so the clip stays
    /// one segment; only if the encoder refuses is the pipeline restarted,
    /// which starts a new segment. Other settings apply from the next
    /// restart.
    pub fn set_video_settings(&mut self, settings: &VideoConfig) {
        self.settings = settings.clone();
        let Some(video) = &self.video else {
            return;
        };
        let f = &video.info().format;
        let kbps = settings.bitrate_kbps.unwrap_or_else(|| {
            default_bitrate_kbps(settings.quality, settings.codec, f.width, f.height, f.fps)
        });
        if kbps == self.bitrate_kbps {
            return;
        }
        match video.set_bitrate(kbps) {
            Ok(actual) => {
                info!(
                    "{}: video bitrate {} -> {actual} kbit/s",
                    self.target.game, self.bitrate_kbps
                );
                self.bitrate_kbps = actual;
                self.description = describe(video, actual);
            }
            Err(e) => {
                warning!("{}: bitrate change refused ({e})", self.target.game);
                self.restart_video(qpc_now(), Some("new bitrate"));
            }
        }
    }

    /// The video bitrate in use, in kbit/s.
    pub fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps
    }

    /// Per audio input: its name, the setting it records alone, and whether
    /// it is muted.
    pub fn audio_inputs(&self) -> Vec<(&'static str, Option<AudioSwitch>, bool)> {
        let Some(audio) = &self.audio else {
            return Vec::new();
        };
        (self.audio_inputs.iter().enumerate())
            .map(|(i, &(name, switch))| (name, switch, audio.is_muted(i)))
            .collect()
    }

    /// Mutes or unmutes the input that records `switch` alone. Returns
    /// whether this session has one.
    pub fn set_switch(&mut self, switch: AudioSwitch, on: bool) -> bool {
        let input = (self.audio_inputs.iter()).position(|&(_, s)| s == Some(switch));
        match (input, &self.audio) {
            (Some(input), Some(audio)) => {
                audio.set_muted(input, !on);
                true
            }
            _ => false,
        }
    }

    /// Mutes or unmutes audio input `input` for this session only.
    pub fn set_input_muted(&mut self, input: usize, muted: bool) {
        if let Some(audio) = &self.audio {
            audio.set_muted(input, muted);
        }
    }

    pub fn buffer(&self) -> Arc<Mutex<ReplayBuffer>> {
        self.buffer.clone()
    }

    pub fn tracks(&self) -> Vec<TrackInfo> {
        self.tracks.clone()
    }

    /// Every video segment started so far with its format, oldest first.
    pub fn segments(&self) -> Vec<(u32, VideoFormat)> {
        self.segments.clone()
    }

    /// Short description of the pipeline, e.g. "HEVC 2560x1440@60 18 Mbit/s · AMD Radeon RX 7800 XT".
    pub fn describe(&self) -> String {
        self.description.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_scales_sensibly() {
        let b = |q, c, w, h, fps| default_bitrate_kbps(q, c, w, h, fps);
        assert_eq!(b(Quality::Balanced, Codec::Hevc, 1920, 1080, 60), 18_000);
        assert!(b(Quality::Balanced, Codec::Hevc, 2560, 1440, 60) > 18_000);
        assert!(b(Quality::Balanced, Codec::H264, 1920, 1080, 60) > 18_000);
        assert!(b(Quality::Performance, Codec::Hevc, 1920, 1080, 30) < 10_000);
        assert_eq!(b(Quality::Quality, Codec::H264, 7680, 4320, 240), 150_000);
        assert_eq!(b(Quality::Performance, Codec::Hevc, 320, 240, 10), 3_000);
    }

    fn kinds(plan: &AudioPlan) -> Vec<(TrackId, &'static str, bool)> {
        plan.names
            .iter()
            .map(|&(id, name)| {
                let mixed = plan
                    .config
                    .inputs
                    .iter()
                    .find(|i| i.track == Some(id))
                    .is_some_and(|i| i.mix);
                (id, name, mixed)
            })
            .collect()
    }

    #[test]
    fn default_audio_is_game_and_voice_plus_a_mix() {
        let plan = plan_audio(&AudioConfig::default(), Some(42), true).unwrap();
        assert_eq!(
            kinds(&plan),
            vec![
                (1, "Mixed", false),
                (2, "Game", true),
                (3, "Voice chat", true)
            ]
        );
        assert_eq!(plan.config.mix_track, Some(1));
        assert_eq!(
            plan.config.inputs[0].source,
            AudioSource::Process {
                target: ProcessTarget::Pid(42),
                exclude: false
            }
        );
    }

    #[test]
    fn other_apps_replaces_voice_chat_in_the_mix() {
        let a = AudioConfig {
            other_apps: true,
            microphone: true,
            ..AudioConfig::default()
        };
        let plan = plan_audio(&a, Some(42), true).unwrap();
        assert_eq!(
            kinds(&plan),
            vec![
                (1, "Mixed", false),
                (2, "Game", true),
                (3, "Voice chat", false),
                (4, "Other apps", true),
                (5, "Microphone", true)
            ]
        );
    }

    #[test]
    fn without_per_app_audio_one_loopback_track_stands_in() {
        let plan = plan_audio(&AudioConfig::default(), Some(42), false).unwrap();
        assert_eq!(kinds(&plan), vec![(1, "Game & system audio", false)]);
        assert_eq!(plan.config.mix_track, None);
        let desktop = plan_audio(
            &AudioConfig {
                microphone: true,
                ..AudioConfig::default()
            },
            None,
            true,
        )
        .unwrap();
        assert_eq!(
            kinds(&desktop),
            vec![
                (1, "Mixed", false),
                (2, "System audio", true),
                (3, "Microphone", true)
            ]
        );
    }

    #[test]
    fn single_input_or_mix_off_writes_no_mix() {
        let a = AudioConfig {
            voice_chat: false,
            ..AudioConfig::default()
        };
        assert_eq!(
            kinds(&plan_audio(&a, Some(1), true).unwrap()),
            vec![(1, "Game", false)]
        );
        let a = AudioConfig {
            mixed_track: false,
            ..AudioConfig::default()
        };
        assert_eq!(
            kinds(&plan_audio(&a, Some(1), true).unwrap()),
            vec![(1, "Game", false), (2, "Voice chat", false)]
        );
        let none = AudioConfig {
            game: false,
            voice_chat: false,
            ..AudioConfig::default()
        };
        assert_eq!(plan_audio(&none, Some(1), true), None);
    }
}
