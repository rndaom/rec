//! One recording session: the video and audio pipelines for one target, all
//! feeding a shared replay buffer of encoded packets.

use std::sync::{Arc, Mutex};

use rec_audio::{AudioCapture, AudioCaptureConfig, AudioInput, AudioSource, ProcessTarget};
use rec_core::config::{AudioConfig, Codec, Config, Quality};
use rec_core::media::{TrackInfo, TrackKind, VIDEO_TRACK};
use rec_core::replay::{Limits, Packet, ReplayBuffer, TrackId};
use rec_core::time::{qpc_now, secs};
use rec_video::{CaptureSource, CodecPreference, VideoCapture, VideoCaptureConfig};
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
    SourceClosed,
    Failed(String),
}

#[derive(Debug)]
pub struct Session {
    pub target: Target,
    buffer: Arc<Mutex<ReplayBuffer>>,
    tracks: Vec<TrackInfo>,
    description: String,
    // Video is declared before audio so it stops first when dropped.
    video: VideoCapture,
    _audio: Option<AudioCapture>,
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

/// The audio inputs and tracks a session records, with each track's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioPlan {
    pub config: AudioCaptureConfig,
    /// `(track, name)` in track order, mixed track first.
    pub names: Vec<(TrackId, &'static str)>,
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
    // (source, name, in the mix)
    let mut inputs: Vec<(AudioSource, &'static str, bool)> = Vec::new();
    match game_pid.filter(|_| per_app) {
        Some(pid) => {
            if a.game {
                inputs.push((process(ProcessTarget::Pid(pid), false), "Game", true));
            }
            if a.voice_chat && !a.voice_chat_apps.is_empty() {
                let apps = ProcessTarget::Executables(a.voice_chat_apps.clone());
                inputs.push((process(apps, false), "Voice chat", !a.other_apps));
            }
            if a.other_apps {
                inputs.push((process(ProcessTarget::Pid(pid), true), "Other apps", true));
            }
        }
        None => {
            if a.game || a.voice_chat || a.other_apps {
                let name = if game_pid.is_some() {
                    "Game & system audio"
                } else {
                    "System audio"
                };
                inputs.push((AudioSource::SystemLoopback, name, true));
            }
        }
    }
    if a.microphone {
        let mic = AudioSource::Microphone {
            device_id: a.microphone_device.clone(),
        };
        inputs.push((mic, "Microphone", true));
    }
    if inputs.is_empty() {
        return None;
    }

    let mixed = inputs.iter().filter(|(_, _, mix)| *mix).count();
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
        .map(|(source, name, in_mix)| {
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
    })
}

/// Output size after the `max_height` cap, keeping the aspect ratio.
fn output_size(width: u32, height: u32, max_height: Option<u32>) -> (u32, u32) {
    match max_height {
        Some(max) if height > max => (width * max / height, max),
        _ => (width, height),
    }
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
        let codec = match v.codec {
            Codec::Auto => CodecPreference::Auto,
            Codec::H264 => CodecPreference::H264,
            Codec::Hevc => CodecPreference::Hevc,
            Codec::Av1 => {
                warning!("AV1 encoding is not available yet; using HEVC/H.264");
                CodecPreference::Auto
            }
        };
        let (src_w, src_h) = source_size(source, target.hwnd);
        let (out_w, out_h) = output_size(src_w, src_h, v.max_height);
        let bitrate_kbps = v
            .bitrate_kbps
            .unwrap_or_else(|| default_bitrate_kbps(v.quality, v.codec, out_w, out_h, v.fps));

        let needed = u64::from(bitrate_kbps) * 1000 / 8 * u64::from(seconds);
        if needed > max_bytes as u64 {
            warning!(
                "{} s at {bitrate_kbps} kbit/s needs ~{} MB but the buffer is capped at {} MB; clips may be shorter",
                seconds,
                needed / (1024 * 1024),
                replay.max_memory_mb
            );
        }

        let video_config = VideoCaptureConfig {
            source,
            fps: v.fps,
            max_height: v.max_height,
            codec,
            bitrate_kbps,
            cursor: v.cursor,
            track: VIDEO_TRACK,
            segment: 0,
        };
        let video = VideoCapture::start(video_config, sink(&buffer)).map_err(|e| e.to_string())?;
        let info = video.info();
        let format = info.format.clone();
        let description = format!(
            "{} {}x{}@{} {} Mbit/s · {}",
            format.codec.name(),
            format.width,
            format.height,
            format.fps,
            bitrate_kbps / 1000,
            info.adapter_name
        );

        let mut tracks = vec![TrackInfo {
            id: VIDEO_TRACK,
            name: "Video".into(),
            kind: TrackKind::Video(format),
        }];

        // ---- audio ----
        let per_app = rec_audio::process_loopback_supported();
        let game_pid = (target.pid != 0).then_some(target.pid);
        let mut audio = None;
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
                }
                Err(e) => warning!("audio unavailable, recording without it: {e}"),
            }
        }
        let audio_tracks = tracks.len() - 1;

        info!(
            "session: {description}, encoder {}, rate control {}, {} audio track(s), buffer {seconds} s / {} MB",
            info.encoder_name, info.rate_control, audio_tracks, replay.max_memory_mb
        );
        Ok(Self {
            target,
            buffer,
            tracks,
            description,
            video,
            _audio: audio,
            watcher: Watcher::new(qpc_now()),
            problem: None,
        })
    }

    /// Checks that the game is really being captured (see [`crate::watch`]).
    /// Returns the new state when it changed.
    pub fn watch(&mut self) -> Option<Option<Problem>> {
        let hwnd = self.target.hwnd;
        if hwnd.0 == 0 {
            // Desktop recording: a still desktop sends no frames, rightly.
            return None;
        }
        let stats = self.video.stats();
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

    /// The capture problem the watcher currently sees, if any.
    pub fn problem(&self) -> Option<Problem> {
        self.problem
    }

    pub fn health(&self) -> Health {
        let stats = self.video.stats();
        if stats.source_closed {
            Health::SourceClosed
        } else if !stats.running {
            Health::Failed(
                stats
                    .last_error
                    .unwrap_or_else(|| "video capture stopped".into()),
            )
        } else {
            Health::Ok
        }
    }

    pub fn buffer(&self) -> Arc<Mutex<ReplayBuffer>> {
        self.buffer.clone()
    }

    pub fn tracks(&self) -> Vec<TrackInfo> {
        self.tracks.clone()
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

    #[test]
    fn output_size_caps_height() {
        assert_eq!(output_size(2560, 1440, Some(1080)), (1920, 1080));
        assert_eq!(output_size(1920, 1080, Some(1440)), (1920, 1080));
        assert_eq!(output_size(1920, 1080, None), (1920, 1080));
    }
}
