//! The service's main thread.
//!
//! A hidden top-level window owns the tray icon, hotkeys, foreground tracking
//! and the recording session. Everything that happens (window messages,
//! foreground changes, IPC requests from other threads, finished saves)
//! becomes an [`Event`] handled here one at a time, so no state is shared
//! across threads except the replay buffer inside the session.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use rec_core::config::{self, CaptureTarget, Config, MAX_REPLAY_SECONDS, RecordWhen};
use rec_core::ipc::{PROTOCOL_VERSION, RecorderState, Request, Response, Status};
use rec_core::time::{as_secs_f64, qpc_now, secs};
use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTOPRIMARY, MonitorFromWindow};
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFShutdown, MFStartup};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Shell::{NIN_SELECT, NINF_KEY, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, EVENT_SYSTEM_FOREGROUND,
    GetForegroundWindow, GetMessageW, IsWindow, KillTimer, MSG, PostMessageW, PostQuitMessage,
    RegisterClassW, RegisterWindowMessageW, SW_SHOWNORMAL, SetTimer, TranslateMessage,
    WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_APP, WM_CONTEXTMENU, WM_ENDSESSION,
    WM_HOTKEY, WM_QUERYENDSESSION, WM_RBUTTONUP, WM_TIMER, WNDCLASSW, WS_OVERLAPPED,
};
use windows::core::{PCWSTR, w};

use crate::detect;
use crate::feedback::{self, Kind, Toast};
use crate::hotkeys::{Action, Hotkeys};
use crate::ipc_server::IpcServer;
use crate::log::{error, info, warning};
use crate::paths::{self, Stamp};
use crate::saver::{self, SaveJob};
use crate::session::{Health, Session, Target};
use crate::tray::{Look, MenuCommand, Tray, WM_TRAY};
use crate::win::{Hwnd, wide};

const WM_WAKE: u32 = WM_APP + 2;
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;
const TIMER_HEALTH: usize = 1;
const TIMER_TOAST: usize = 2;
const HEALTH_INTERVAL_MS: u32 = 2000;
const TOAST_MS: u32 = 2500;
/// Don't retry a target that failed to start more often than this.
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// Retry delays after consecutive failures to start on the same target.
/// Games often create their window and then switch it to fullscreen; a
/// capture started in between can fail, and works a moment later.
const QUICK_RETRIES: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
];

/// A target that failed to record.
#[derive(Clone, Copy, Debug)]
struct Failure {
    hwnd: Hwnd,
    at: Instant,
    /// Consecutive failures on this target.
    count: usize,
}

impl Failure {
    fn retry_after(&self) -> Duration {
        QUICK_RETRIES
            .get(self.count.saturating_sub(1))
            .copied()
            .unwrap_or(RETRY_AFTER)
    }

    fn retry_due(&self) -> bool {
        self.at.elapsed() >= self.retry_after()
    }
}

#[derive(Debug)]
pub enum Event {
    Hotkey(i32),
    /// Notification-icon event (the low word of the callback's lParam).
    Tray(u32),
    TaskbarCreated,
    Timer(usize),
    Foreground(Hwnd),
    Ipc(Request, Sender<Response>),
    Saved {
        result: Result<PathBuf, String>,
        seconds: u32,
        game: String,
        reply: Option<Sender<Response>>,
    },
    Shutdown,
}

/// Sends events to the main thread from anywhere and wakes its message loop.
#[derive(Clone, Debug)]
pub struct Events {
    tx: Sender<Event>,
    hwnd: Hwnd,
}

impl Events {
    pub fn send(&self, event: Event) {
        if self.tx.send(event).is_ok() {
            // SAFETY: posting to our own window; harmless if it is gone.
            let _ = unsafe { PostMessageW(Some(self.hwnd.get()), WM_WAKE, WPARAM(0), LPARAM(0)) };
        }
    }
}

thread_local! {
    static EVENTS: RefCell<Option<Events>> = const { RefCell::new(None) };
}
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

fn post(event: Event) {
    EVENTS.with(|events| {
        if let Some(events) = &*events.borrow() {
            events.send(event);
        }
    });
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let event = match msg {
        WM_HOTKEY => Some(Event::Hotkey(wparam.0 as i32)),
        WM_TRAY => Some(Event::Tray(lparam.0 as u32 & 0xFFFF)),
        WM_TIMER => Some(Event::Timer(wparam.0)),
        WM_QUERYENDSESSION => return LRESULT(1),
        WM_ENDSESSION if wparam.0 != 0 => Some(Event::Shutdown),
        WM_WAKE => return LRESULT(0),
        m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => Some(Event::TaskbarCreated),
        _ => None,
    };
    match event {
        Some(event) => {
            post(event);
            LRESULT(0)
        }
        // SAFETY: forwarding the original arguments.
        None => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe extern "system" fn on_foreground(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    post(Event::Foreground(hwnd.into()));
}

/// Runs the service until the user quits. Returns early (successfully) if
/// another instance is already running.
pub fn run() -> Result<(), String> {
    // SAFETY: process-wide setup calls made once, before any window exists.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .map_err(|e| format!("COM init failed: {e}"))?;
    }

    // SAFETY: named mutex for single-instance detection; the handle lives
    // until the process exits.
    let _instance =
        unsafe { CreateMutexW(None, true, w!("Local\\rec.recd")) }.map_err(|e| e.to_string())?;
    // SAFETY: reads the calling thread's last error, set by CreateMutexW.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        info!("another recd is already running; exiting");
        return Ok(());
    }

    // SAFETY: paired with MFShutdown below.
    let media_foundation = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) };
    if let Err(e) = &media_foundation {
        error!("Media Foundation unavailable: {e}");
    }

    let hwnd = create_window()?;
    let (tx, rx) = mpsc::channel();
    let events = Events { tx, hwnd };
    EVENTS.with(|e| *e.borrow_mut() = Some(events.clone()));

    // SAFETY: out-of-context hook (no DLL injection); callbacks arrive
    // through this thread's message loop.
    let hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(on_foreground),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };

    let mut app = App::new(hwnd, events, media_foundation.is_ok());
    app.startup();

    let mut msg = MSG::default();
    // SAFETY: standard message loop on the thread that owns the window.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            while let Ok(event) = rx.try_recv() {
                if app.handle(event) == Flow::Quit {
                    PostQuitMessage(0);
                }
            }
        }
    }

    app.shutdown();
    drop(app);
    // SAFETY: undoing the setup above.
    unsafe {
        let _ = UnhookWinEvent(hook);
        if media_foundation.is_ok() {
            let _ = MFShutdown();
        }
    }
    info!("recd stopped");
    Ok(())
}

fn create_window() -> Result<Hwnd, String> {
    // SAFETY: registers a class with a valid window procedure and creates a
    // never-shown top-level window (top-level so it receives the
    // TaskbarCreated broadcast when Explorer restarts).
    unsafe {
        let instance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("rec.recd"),
            ..Default::default()
        };
        RegisterClassW(&class);
        TASKBAR_CREATED.store(
            RegisterWindowMessageW(w!("TaskbarCreated")),
            Ordering::Relaxed,
        );
        let hwnd = CreateWindowExW(
            Default::default(),
            w!("rec.recd"),
            w!("rec"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .map_err(|e| format!("could not create window: {e}"))?;
        Ok(hwnd.into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    Quit,
}

struct App {
    hwnd: Hwnd,
    events: Events,
    config: Config,
    config_path: PathBuf,
    /// Problem with the settings file, shown until fixed.
    config_error: Option<String>,
    first_run: bool,
    media_foundation: bool,
    tray: Tray,
    toast: Option<Toast>,
    hotkeys: Hotkeys,
    ipc: Option<IpcServer>,
    session: Option<Session>,
    paused: bool,
    /// Why the last session stopped or failed to start.
    error: Option<String>,
    failed: Option<Failure>,
    own_pid: u32,
}

impl App {
    fn new(hwnd: Hwnd, events: Events, media_foundation: bool) -> Self {
        let config_path = config::default_config_path();
        let first_run = !config_path.exists();
        let (config, config_error) = match Config::load(&config_path) {
            Ok(config) => (config, None),
            Err(e) => {
                error!("{e}");
                (Config::default(), Some(e.to_string()))
            }
        };
        if first_run && let Err(e) = config.save(&config_path) {
            warning!("could not write default config: {e}");
        }

        let toast = Toast::new()
            .map_err(|e| warning!("toast window unavailable: {e}"))
            .ok();
        let ipc = IpcServer::start(events.clone())
            .map_err(|e| warning!("IPC unavailable: {e}"))
            .ok();

        Self {
            hwnd,
            events,
            config,
            config_path,
            config_error,
            first_run,
            media_foundation,
            tray: Tray::new(hwnd),
            toast,
            hotkeys: Hotkeys::default(),
            ipc,
            session: None,
            paused: false,
            error: None,
            failed: None,
            own_pid: detect::own_pid(),
        }
    }

    fn startup(&mut self) {
        info!("recd {} starting", env!("CARGO_PKG_VERSION"));
        self.hotkeys.register(self.hwnd, &self.config.hotkeys);
        // SAFETY: periodic timer on our own window.
        unsafe {
            SetTimer(
                Some(self.hwnd.get()),
                TIMER_HEALTH,
                HEALTH_INTERVAL_MS,
                None,
            )
        };

        if !self.media_foundation {
            self.error = Some(
                "Windows media components are missing (install the Media Feature Pack)".into(),
            );
        }
        if let Some(e) = &self.config_error {
            self.notify(Kind::Error, "Settings file has an error", &e.clone());
        } else if !self.hotkeys.failures.is_empty() {
            let failures = self.hotkeys.failures.join("; ");
            self.notify(Kind::Error, "Hotkey unavailable", &failures);
        } else if self.first_run {
            let detail = format!(
                "Press {} in game to save the last {} s",
                self.config.hotkeys.save_clip, self.config.replay.seconds
            );
            self.notify(Kind::Info, "rec is running", &detail);
        }
        self.kick();
        self.refresh();
    }

    fn shutdown(&mut self) {
        // Nothing answers requests any more: refuse them right away.
        if let Some(ipc) = &self.ipc {
            ipc.close();
        }
        self.stop_session("shutting down");
        self.hotkeys.unregister();
        self.ipc = None;
    }

    // ---- state ---------------------------------------------------------------

    fn state(&self) -> (RecorderState, Option<String>) {
        if self.paused {
            (RecorderState::Paused, Some("Paused".into()))
        } else if self.session.is_some() {
            (RecorderState::Recording, None)
        } else if let Some(e) = &self.error {
            (RecorderState::Error, Some(e.clone()))
        } else {
            let reason = match self.config.behavior.record_when {
                RecordWhen::GameDetected => "Waiting for a game",
                RecordWhen::Always => "Starting",
                RecordWhen::Manual => "Manual mode: resume from the tray to record",
            };
            (RecorderState::Idle, Some(reason.into()))
        }
    }

    fn headline(&self) -> String {
        match (&self.session, self.state()) {
            (Some(s), (RecorderState::Recording, _)) => match s.problem() {
                Some(p) => format!("Recording {} · {}", s.target.game, p.short()),
                None => format!(
                    "Recording {} · {} s buffer",
                    s.target.game, self.config.replay.seconds
                ),
            },
            (_, (RecorderState::Error, Some(reason))) => format!("Not recording: {reason}"),
            (_, (_, Some(reason))) => reason,
            _ => String::new(),
        }
    }

    fn refresh(&mut self) {
        let problem = self.session.as_ref().and_then(Session::problem);
        let look = match self.state().0 {
            RecorderState::Recording if problem.is_some() => Look::Error,
            RecorderState::Recording => Look::Recording,
            RecorderState::Idle => Look::Idle,
            RecorderState::Paused => Look::Paused,
            RecorderState::Error => Look::Error,
        };
        let tip = format!("rec · {}", self.headline());
        self.tray.show(look, &tip);
    }

    fn status(&self) -> Status {
        let (state, reason) = self.state();
        let (buffered_seconds, buffer_bytes) = self.session.as_ref().map_or((0.0, 0), |s| {
            s.buffer()
                .lock()
                .map(|b| (as_secs_f64(b.buffered_duration()), b.bytes() as u64))
                .unwrap_or_default()
        });
        Status {
            protocol: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").into(),
            state,
            reason,
            target: self.session.as_ref().map(|s| s.target.game.clone()),
            encoder: self.session.as_ref().map(Session::describe),
            buffered_seconds,
            buffer_bytes,
            warning: self
                .session
                .as_ref()
                .and_then(Session::problem)
                .map(|p| p.short().into()),
        }
    }

    fn notify(&mut self, kind: Kind, title: &str, detail: &str) {
        if self.config.behavior.save_sound || kind == Kind::Error {
            feedback::play(kind);
        }
        if !self.config.behavior.save_toast && kind == Kind::Success {
            return;
        }
        let monitor = self.session.as_ref().and_then(|s| {
            // SAFETY: MonitorFromWindow accepts stale handles.
            let m: HMONITOR =
                unsafe { MonitorFromWindow(s.target.hwnd.get(), MONITOR_DEFAULTTOPRIMARY) };
            (!m.is_invalid()).then_some(m)
        });
        if let Some(toast) = &self.toast {
            toast.show(kind, title, detail, monitor);
            // SAFETY: one-shot timer on our own window, restarted per toast.
            unsafe { SetTimer(Some(self.hwnd.get()), TIMER_TOAST, TOAST_MS, None) };
        }
    }

    // ---- sessions --------------------------------------------------------------

    fn start_session(&mut self, target: Target) {
        self.stop_session("switching target");
        match Session::start(&self.config, target.clone()) {
            Ok(session) => {
                info!(
                    "recording {} ({}): {}",
                    target.game,
                    target.exe_name,
                    session.describe()
                );
                self.session = Some(session);
                self.error = None;
                self.failed = None;
            }
            Err(e) => {
                error!("could not record {}: {e}", target.game);
                let count = self
                    .failed
                    .filter(|f| f.hwnd.0 == target.hwnd.0)
                    .map_or(1, |f| f.count + 1);
                self.failed = Some(Failure {
                    hwnd: target.hwnd,
                    at: Instant::now(),
                    count,
                });
                self.error = Some(e.clone());
                // The first failure is often transient (see QUICK_RETRIES):
                // only speak up when the quick retry failed too.
                if count >= 2 {
                    self.notify(Kind::Error, &format!("Can't record {}", target.game), &e);
                }
            }
        }
        self.refresh();
    }

    fn stop_session(&mut self, why: &str) {
        if let Some(session) = self.session.take() {
            info!("stopped recording {} ({why})", session.target.game);
            drop(session);
        }
    }

    /// Starts recording if the current mode and foreground window call for it.
    fn kick(&mut self) {
        if self.paused || !self.media_foundation || self.session.is_some() {
            return;
        }
        match self.config.behavior.record_when {
            RecordWhen::GameDetected => {
                // SAFETY: no preconditions.
                let foreground = unsafe { GetForegroundWindow() };
                self.consider(foreground.into());
            }
            RecordWhen::Always => {
                let retry_ok = self.failed.is_none_or(|f| f.retry_due());
                if retry_ok {
                    self.start_session(Target {
                        hwnd: Hwnd::default(),
                        game: "Desktop".into(),
                        exe_name: String::new(),
                        pid: 0,
                        display: true,
                    });
                }
            }
            RecordWhen::Manual => {}
        }
    }

    /// Called when `hwnd` becomes the foreground window.
    fn consider(&mut self, hwnd: Hwnd) {
        if self.paused
            || !self.media_foundation
            || self.config.behavior.record_when != RecordWhen::GameDetected
        {
            return;
        }
        let Some(info) = detect::window_info(hwnd.get()) else {
            return;
        };
        if !detect::is_game(&info, &self.config.behavior, self.own_pid) {
            return;
        }
        info!("game window: {} \"{}\"", info.exe_name, info.title);
        if self
            .session
            .as_ref()
            .is_some_and(|s| s.target.hwnd.0 == info.hwnd)
        {
            return;
        }
        if self
            .failed
            .is_some_and(|f| f.hwnd.0 == info.hwnd && !f.retry_due())
        {
            return;
        }
        let target = Target {
            hwnd: Hwnd(info.hwnd),
            game: detect::display_name(&info.exe_path, &info.title),
            exe_name: info.exe_name,
            pid: info.pid,
            display: self.config.video.target == CaptureTarget::Display,
        };
        self.start_session(target);
    }

    fn check_health(&mut self) {
        let Some(session) = &self.session else {
            self.kick();
            return;
        };
        let hwnd = session.target.hwnd;
        let game = session.target.game.clone();
        // SAFETY: IsWindow accepts any handle value.
        let window_gone = hwnd.0 != 0 && !unsafe { IsWindow(Some(hwnd.get())) }.as_bool();
        match session.health() {
            Health::Ok if !window_gone => {
                if let Some(Some(problem)) = self.session.as_mut().and_then(Session::watch) {
                    let (title, detail) = problem.describe(&game);
                    self.notify(Kind::Error, &title, &detail);
                }
            }
            Health::Ok | Health::SourceClosed => {
                self.stop_session("game closed");
                self.error = None;
                self.kick();
            }
            Health::Failed(reason) => {
                self.stop_session(&reason);
                self.failed = Some(Failure {
                    hwnd,
                    at: Instant::now(),
                    count: QUICK_RETRIES.len() + 1,
                });
                self.error = Some(reason.clone());
                self.notify(Kind::Error, &format!("Recording {game} stopped"), &reason);
            }
        }
        self.refresh();
    }

    fn set_paused(&mut self, paused: bool) {
        if paused == self.paused {
            return;
        }
        self.paused = paused;
        if paused {
            self.stop_session("paused");
        } else {
            self.error = None;
            self.failed = None;
            self.kick();
        }
        self.refresh();
    }

    fn reload(&mut self) -> Result<(), String> {
        let config = Config::load(&self.config_path).map_err(|e| e.to_string());
        let config = match config {
            Ok(config) => config,
            Err(e) => {
                self.config_error = Some(e.clone());
                self.notify(Kind::Error, "Settings file has an error", &e);
                return Err(e);
            }
        };
        self.config_error = None;
        self.config = config;
        self.hotkeys.register(self.hwnd, &self.config.hotkeys);
        if !self.hotkeys.failures.is_empty() {
            let failures = self.hotkeys.failures.join("; ");
            self.notify(Kind::Error, "Hotkey unavailable", &failures);
        }
        if let Some(session) = self.session.take() {
            let target = session.target.clone();
            drop(session);
            self.start_session(target);
        } else {
            self.kick();
        }
        info!("settings reloaded");
        self.refresh();
        Ok(())
    }

    // ---- saving ----------------------------------------------------------------

    fn save(&mut self, seconds: Option<u32>, reply: Option<Sender<Response>>) {
        let Some(session) = &self.session else {
            let (_, reason) = self.state();
            let reason = reason.unwrap_or_default();
            if let Some(reply) = reply {
                let _ = reply.send(Response::Error {
                    message: format!("not recording ({reason})"),
                });
            }
            self.notify(
                Kind::Error,
                "Nothing to save",
                &format!("rec isn't recording: {reason}"),
            );
            return;
        };

        let seconds = seconds
            .unwrap_or(self.config.replay.seconds)
            .clamp(1, MAX_REPLAY_SECONDS);
        let now = qpc_now();
        let game = session.target.game.clone();
        let mut dir = paths::clips_dir(&self.config);
        if self.config.output.folder_per_game {
            dir.push(paths::sanitize(&game));
        }
        let job = SaveJob {
            buffer: session.buffer(),
            tracks: session.tracks(),
            start: now - secs(i64::from(seconds)),
            end: now + secs(i64::from(self.config.replay.post_roll_seconds)),
            dir,
            name: paths::render_filename(
                &self.config.output.filename,
                &game,
                Stamp::now(),
                seconds,
            ),
        };
        let events = self.events.clone();
        saver::spawn(job, move |result| {
            events.send(Event::Saved {
                result,
                seconds,
                game,
                reply,
            })
        });
    }

    fn saved(
        &mut self,
        result: Result<PathBuf, String>,
        seconds: u32,
        game: &str,
        reply: Option<Sender<Response>>,
    ) {
        let response = match result {
            Ok(path) => {
                info!("saved {}", path.display());
                self.notify(
                    Kind::Success,
                    "Clip saved",
                    &format!("{seconds} s · {game}"),
                );
                Response::Saved { path }
            }
            Err(e) => {
                error!("save failed: {e}");
                self.notify(Kind::Error, "Couldn't save clip", &e);
                Response::Error { message: e }
            }
        };
        if let Some(reply) = reply {
            let _ = reply.send(response);
        }
    }

    // ---- events ----------------------------------------------------------------

    fn handle(&mut self, event: Event) -> Flow {
        match event {
            Event::Hotkey(id) => match self.hotkeys.action(id) {
                Some(Action::SaveClip) => self.save(None, None),
                Some(Action::SaveSeconds(seconds)) => self.save(Some(seconds), None),
                Some(Action::TogglePause) => {
                    self.set_paused(!self.paused);
                    let title = if self.paused {
                        "Recording paused"
                    } else {
                        "Recording resumed"
                    };
                    self.notify(Kind::Info, title, &self.headline());
                }
                Some(Action::ToggleOverlay) => {
                    let detail = self
                        .session
                        .as_ref()
                        .map_or_else(String::new, Session::describe);
                    self.notify(Kind::Info, &self.headline(), &detail);
                }
                Some(Action::AddMarker) => {
                    self.notify(Kind::Info, "Markers", "Coming in a later version")
                }
                None => {}
            },
            Event::Tray(code) => {
                if [WM_CONTEXTMENU, NIN_SELECT, NIN_KEYSELECT, WM_RBUTTONUP].contains(&code) {
                    let command = self.tray.menu(&self.headline(), self.paused);
                    if let Some(command) = command {
                        return self.menu(command);
                    }
                }
            }
            Event::TaskbarCreated => {
                self.tray.reset();
                self.refresh();
            }
            Event::Timer(TIMER_HEALTH) => self.check_health(),
            Event::Timer(TIMER_TOAST) => {
                // SAFETY: stopping our own timer.
                let _ = unsafe { KillTimer(Some(self.hwnd.get()), TIMER_TOAST) };
                if let Some(toast) = &self.toast {
                    toast.hide();
                }
            }
            Event::Timer(_) => {}
            Event::Foreground(hwnd) => {
                self.consider(hwnd);
            }
            Event::Ipc(request, reply) => return self.ipc(request, reply),
            Event::Saved {
                result,
                seconds,
                game,
                reply,
            } => self.saved(result, seconds, &game, reply),
            Event::Shutdown => return Flow::Quit,
        }
        Flow::Continue
    }

    fn ipc(&mut self, request: Request, reply: Sender<Response>) -> Flow {
        let response = match request {
            Request::Status => Response::Status(self.status()),
            Request::Save { seconds } => {
                self.save(seconds, Some(reply));
                return Flow::Continue;
            }
            Request::Mark => Response::Error {
                message: "markers are not supported yet".into(),
            },
            Request::Pause => {
                self.set_paused(true);
                Response::Ok
            }
            Request::Resume => {
                self.set_paused(false);
                Response::Ok
            }
            Request::ReloadConfig => match self.reload() {
                Ok(()) => Response::Ok,
                Err(message) => Response::Error { message },
            },
            Request::Quit => {
                let _ = reply.send(Response::Ok);
                return Flow::Quit;
            }
        };
        let _ = reply.send(response);
        Flow::Continue
    }

    fn menu(&mut self, command: MenuCommand) -> Flow {
        match command {
            MenuCommand::SaveClip => self.save(None, None),
            MenuCommand::TogglePause => self.set_paused(!self.paused),
            MenuCommand::OpenClips => {
                let dir = paths::clips_dir(&self.config);
                let _ = std::fs::create_dir_all(&dir);
                open(&dir);
            }
            MenuCommand::OpenConfig => {
                if !self.config_path.exists() {
                    let _ = self.config.save(&self.config_path);
                }
                open_text(&self.config_path);
            }
            MenuCommand::OpenLog => open_text(&paths::data_dir().join("recd.log")),
            MenuCommand::Quit => return Flow::Quit,
        }
        Flow::Continue
    }
}

fn shell_open(file: &Path, params: Option<&Path>) -> bool {
    let file = wide(file);
    let params = params.map(wide);
    // SAFETY: NUL-terminated strings that outlive the call.
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(file.as_ptr()),
            params
                .as_ref()
                .map_or(PCWSTR::null(), |p| PCWSTR(p.as_ptr())),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    result.0 as isize > 32
}

fn open(path: &Path) {
    if !shell_open(path, None) {
        warning!("could not open {}", path.display());
    }
}

/// Opens a text file with its associated app, falling back to Notepad
/// (`.toml` and `.log` often have no association).
fn open_text(path: &Path) {
    if !shell_open(path, None) && !shell_open(Path::new("notepad.exe"), Some(path)) {
        warning!("could not open {}", path.display());
    }
}
