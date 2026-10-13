//! Starting, stopping and querying `recd` for the bench.
//!
//! The rules the bench follows: it only ever stops `recd` by asking it to
//! (`Request::Quit` over IPC), and it only ever kills processes it spawned
//! itself (PresentMon). A `recd` that was already running when the bench
//! started is stopped for the "off" phases and restarted at the end.
//!
//! IPC is never used to find out whether `recd` has gone after `Quit`: while
//! it shuts down its pipe can still accept a connection that is never
//! answered, and `ipc::send` has no read timeout. The bench waits on the
//! process instead.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rec_core::ipc::{self, Request, Response, Status};

use crate::sys::{self, Process};

/// How long to wait for `recd` to start answering, reach a state, or exit.
pub const STATE_TIMEOUT: Duration = Duration::from_secs(30);
const EXIT_TIMEOUT: Duration = Duration::from_secs(15);

/// `recd.exe` next to the running `rec-bench.exe`.
pub fn default_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("recd.exe")))
        .unwrap_or_else(|| PathBuf::from("recd.exe"))
}

/// The file name of `path`, as the process list shows it.
pub fn exe_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(|| "recd.exe".into(), |n| n.to_string_lossy().into_owned())
}

/// `recd`'s status, or `None` if it is not running. Any other failure is an
/// error: the bench must not guess whether `recd` is running.
pub fn status() -> Result<Option<Status>, String> {
    match ipc::send(&Request::Status) {
        Ok(Response::Status(status)) => Ok(Some(status)),
        Ok(other) => Err(format!("unexpected answer to `status`: {other:?}")),
        Err(e) if e == ipc::NOT_RUNNING => Ok(None),
        Err(e) => Err(e),
    }
}

/// Path of the executable behind the running `recd` process, so it can be
/// started again from the same place afterwards.
pub fn running_image(recd_path: &Path) -> Option<PathBuf> {
    sys::find_processes(&exe_name(recd_path))
        .into_iter()
        .find_map(|pid| Process::open(pid).ok()?.image_path())
}

/// Id of the process serving the rec pipe, if there is one.
pub fn server_pid() -> Result<Option<u32>, String> {
    sys::pipe_server_pid(&ipc::pipe_name())
}

/// A `recd` process the bench waits for after asking it to quit.
struct ExitWatch {
    pid: u32,
    /// `None` if the process could not be opened for waiting; it is then
    /// found by enumerating processes.
    handle: Option<Process>,
}

impl ExitWatch {
    fn new(pid: u32) -> Self {
        Self {
            pid,
            handle: Process::open_waitable(pid).ok(),
        }
    }

    /// Waits until the process is gone or `deadline` passes; `true` if gone.
    fn wait(&self, exe_name: &str, deadline: Instant) -> Result<bool, String> {
        match &self.handle {
            Some(handle) => handle.wait_exit(deadline.saturating_duration_since(Instant::now())),
            None => loop {
                if !sys::find_processes(exe_name).contains(&self.pid) {
                    return Ok(true);
                }
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                sys::sleep(Duration::from_millis(100))?;
            },
        }
    }
}

/// What a state-wait should do with the status it just saw.
pub enum Readiness {
    Ready,
    Wait,
    Fail(String),
}

/// A `recd` the bench may have started.
#[derive(Debug)]
pub struct Recd {
    path: PathBuf,
    child: Option<Child>,
}

impl Recd {
    pub fn new(path: PathBuf) -> Self {
        Self { path, child: None }
    }

    /// Process id of the running `recd`: the one this bench started, else the
    /// owner of the rec pipe. Only if the pipe cannot be inspected is the
    /// process name used, and then only when exactly one process has it:
    /// sampling some other `recd.exe` would silently give wrong numbers.
    pub fn pid(&self) -> Result<u32, String> {
        if let Some(child) = &self.child {
            return Ok(child.id());
        }
        let name = exe_name(&self.path);
        let why = match server_pid() {
            Ok(Some(pid)) => return Ok(pid),
            Ok(None) => return Err("recd is not running".into()),
            Err(e) => e,
        };
        match sys::find_processes(&name).as_slice() {
            [pid] => Ok(*pid),
            [] => Err(format!(
                "no process named {name} was found (pass --recd with the right file name)"
            )),
            many => Err(format!(
                "{} processes are named {name} ({many:?}) and the one behind the rec pipe could                  not be identified ({why})",
                many.len()
            )),
        }
    }

    /// Spawns `recd` without waiting for it. Its output is discarded: it is a
    /// background service and must not write into the bench's console.
    pub fn start(&mut self) -> Result<(), String> {
        if !self.path.is_file() {
            return Err(format!(
                "recd was not found at {} (pass --recd <path to recd.exe>)",
                self.path.display()
            ));
        }
        let child = Command::new(&self.path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("could not start {}: {e}", self.path.display()))?;
        self.child = Some(child);
        Ok(())
    }

    /// Waits until `check` says the status is what the caller needs.
    pub fn wait_for(
        &mut self,
        timeout: Duration,
        what: &str,
        check: impl Fn(&Status) -> Readiness,
    ) -> Result<Status, String> {
        let deadline = Instant::now() + timeout;
        let mut last = "recd is not answering yet".to_string();
        while Instant::now() < deadline {
            if let Some(child) = &mut self.child {
                if let Ok(Some(exit)) = child.try_wait() {
                    self.child = None;
                    return Err(format!(
                        "recd exited right after starting ({exit}); \
                         see %LOCALAPPDATA%\\rec\\recd.log"
                    ));
                }
            }
            if let Some(status) = status()? {
                match check(&status) {
                    Readiness::Ready => return Ok(status),
                    Readiness::Fail(why) => return Err(why),
                    Readiness::Wait => {
                        last = format!(
                            "state is `{:?}`{}",
                            status.state,
                            status
                                .reason
                                .as_deref()
                                .map_or(String::new(), |r| format!(" ({r})"))
                        );
                    }
                }
            }
            sys::sleep(Duration::from_millis(250))?;
        }
        Err(format!(
            "recd did not become {what} within {} s: {last}",
            timeout.as_secs()
        ))
    }

    /// Asks `recd` to quit and waits for it to be gone. Does nothing if it is
    /// not running.
    pub fn stop(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + EXIT_TIMEOUT;
        // Find the process to wait for before asking it to quit, while the
        // pipe still answers. A child of ours is waited for directly.
        let name = exe_name(&self.path);
        let watch: Vec<ExitWatch> = if self.child.is_some() {
            Vec::new()
        } else {
            match server_pid() {
                Ok(Some(pid)) => vec![ExitWatch::new(pid)],
                Ok(None) => Vec::new(),
                Err(_) => sys::find_processes(&name)
                    .into_iter()
                    .map(ExitWatch::new)
                    .collect(),
            }
        };
        loop {
            match ipc::send(&Request::Quit) {
                Ok(_) => break,
                Err(e) if e == ipc::NOT_RUNNING => {
                    // Not answering: either gone, or one we just spawned that
                    // has not opened its pipe yet.
                    let starting = self
                        .child
                        .as_mut()
                        .is_some_and(|c| matches!(c.try_wait(), Ok(None)));
                    if !starting {
                        self.child = None;
                        return Ok(());
                    }
                    if Instant::now() >= deadline {
                        return Err(
                            "recd never opened its pipe, so it could not be asked to quit".into(),
                        );
                    }
                    sys::sleep(Duration::from_millis(250))?;
                }
                Err(e) => return Err(e),
            }
        }
        // Quit accepted: wait for the process itself, never for the pipe.
        let timed_out = || {
            format!(
                "recd did not exit within {} s after being asked to quit",
                EXIT_TIMEOUT.as_secs()
            )
        };
        if let Some(child) = &mut self.child {
            while !matches!(child.try_wait(), Ok(Some(_))) {
                if Instant::now() >= deadline {
                    return Err(timed_out());
                }
                sys::sleep(Duration::from_millis(100))?;
            }
        } else if watch.is_empty() {
            return Err("recd accepted `quit` but its process could not be identified, so the                  bench cannot tell when it has exited"
                .into());
        } else {
            for target in &watch {
                if !target.wait(&name, deadline)? {
                    return Err(timed_out());
                }
            }
        }
        self.child = None;
        // The process has exited, but give the driver a moment to release the
        // capture and encoder resources before the next phase.
        sys::sleep(Duration::from_millis(500))
    }

    /// Lets go of the `recd` we started without stopping it (used when
    /// restoring the user's own instance).
    pub fn release(&mut self) {
        self.child = None;
    }
}

/// Starts the user's `recd` again from `path` and waits until it answers.
pub fn restore(path: &Path) -> Result<(), String> {
    let mut recd = Recd::new(path.to_owned());
    recd.start()?;
    recd.wait_for(Duration::from_secs(15), "ready", |_| Readiness::Ready)?;
    recd.release();
    Ok(())
}
