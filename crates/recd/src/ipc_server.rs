//! Named-pipe server for `rec-core::ipc`.
//!
//! One pipe instance is served by a background thread, one client at a time
//! (clients are CLIs, the settings UI and macro tools; requests are rare and
//! tiny). The pipe is created with `FILE_FLAG_FIRST_PIPE_INSTANCE`, so no
//! other process can squat on the name, and it rejects remote clients.
//!
//! Once recd starts shutting down ([`IpcServer::close`]) the message loop no
//! longer answers, so requests are refused at once instead of waiting for a
//! reply that never comes.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rec_core::ipc::{self, Request, Response};
use windows::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, HANDLE};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FlushFileBuffers, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::core::PCWSTR;

use crate::app::{Event, Events};
use crate::log::{info, warning};
use crate::win::wide;

const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// Saves with post-roll can take a while; give up eventually.
const REPLY_TIMEOUT: Duration = Duration::from_secs(180);
/// How often a waiting request checks whether recd is shutting down.
const CLOSE_CHECK: Duration = Duration::from_millis(100);
/// How long dropping the server waits for its thread.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTTING_DOWN: &str = "rec is shutting down";

#[derive(Debug)]
pub struct IpcServer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// A pipe handle moved to the server thread.
struct Pipe(isize);

impl Pipe {
    fn handle(&self) -> HANDLE {
        HANDLE(self.0 as *mut _)
    }
}

/// Reads one `\n`-terminated line, or `None` if the client hung up.
fn read_line(pipe: &mut impl Read) -> Option<String> {
    let mut data = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = pipe.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&chunk[..n]);
        if data.contains(&b'\n') || data.len() > MAX_REQUEST_BYTES {
            break;
        }
    }
    let end = data.iter().position(|&b| b == b'\n').unwrap_or(data.len());
    (end > 0).then(|| String::from_utf8_lossy(&data[..end]).into_owned())
}

struct PipeIo<'a>(&'a Pipe);

impl Read for PipeIo<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut read = 0;
        // SAFETY: the buffer is valid for its length; synchronous handle.
        match unsafe { ReadFile(self.0.handle(), Some(buf), Some(&mut read), None) } {
            Ok(()) => Ok(read as usize),
            Err(_) => Ok(0),
        }
    }
}

impl Write for PipeIo<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut written = 0;
        // SAFETY: the buffer is valid for its length; synchronous handle.
        unsafe { WriteFile(self.0.handle(), Some(buf), Some(&mut written), None) }
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(written as usize)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // SAFETY: flushing our own pipe handle.
        unsafe { FlushFileBuffers(self.0.handle()) }
            .map_err(|e| std::io::Error::other(e.to_string()))
    }
}

fn serve(pipe: &Pipe, events: &Events, closing: &AtomicBool) {
    let mut io = PipeIo(pipe);
    let Some(line) = read_line(&mut io) else {
        return;
    };
    let error = |message: &str| Response::Error {
        message: message.into(),
    };
    let response = match ipc::decode_line::<Request>(&line) {
        Ok(_) if closing.load(Ordering::Acquire) => error(SHUTTING_DOWN),
        Ok(request) => {
            let (reply, answer) = mpsc::channel();
            events.send(Event::Ipc(request, reply));
            let start = Instant::now();
            loop {
                match answer.recv_timeout(CLOSE_CHECK) {
                    Ok(response) => break response,
                    Err(mpsc::RecvTimeoutError::Timeout)
                        if !closing.load(Ordering::Acquire) && start.elapsed() < REPLY_TIMEOUT => {}
                    Err(mpsc::RecvTimeoutError::Timeout) if closing.load(Ordering::Acquire) => {
                        break error(SHUTTING_DOWN);
                    }
                    Err(_) => break error("rec did not answer in time"),
                }
            }
        }
        Err(e) => Response::Error {
            message: format!("invalid request: {e}"),
        },
    };
    let _ = io.write_all(ipc::encode_line(&response).as_bytes());
    let _ = io.flush();
}

impl IpcServer {
    pub fn start(events: Events) -> Result<Self, String> {
        let name = ipc::pipe_name();
        let wname = wide(&name);
        // SAFETY: valid NUL-terminated name; default security attributes.
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(wname.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                4096,
                4096,
                0,
                None,
            )
        };
        if handle.is_invalid() {
            return Err(format!(
                "could not create {name}: {}",
                windows::core::Error::from_thread()
            ));
        }
        let pipe = Pipe(handle.0 as isize);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("ipc".into())
            .spawn(move || {
                while !stop_flag.load(Ordering::Acquire) {
                    // SAFETY: our pipe handle, used synchronously on this thread.
                    let connected = match unsafe { ConnectNamedPipe(pipe.handle(), None) } {
                        Ok(()) => true,
                        Err(e) => e.code() == ERROR_PIPE_CONNECTED.to_hresult(),
                    };
                    if stop_flag.load(Ordering::Acquire) {
                        break;
                    }
                    if connected {
                        serve(&pipe, &events, &stop_flag);
                    }
                    // SAFETY: as above.
                    let _ = unsafe { DisconnectNamedPipe(pipe.handle()) };
                }
                // SAFETY: closing the handle this thread owns.
                let _ = unsafe { CloseHandle(pipe.handle()) };
            })
            .map_err(|e| e.to_string())?;
        info!("ipc listening on {name}");
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl IpcServer {
    /// Refuses requests from now on; call when shutdown begins.
    pub fn close(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.close();
        let Some(thread) = self.thread.take() else {
            return;
        };
        // Wake a blocked ConnectNamedPipe by connecting ourselves. While a
        // client is still being served the pipe is busy, so keep trying
        // until the thread has seen the stop flag.
        let deadline = Instant::now() + STOP_TIMEOUT;
        while !thread.is_finished() && Instant::now() < deadline {
            let _ = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(ipc::pipe_name());
            std::thread::sleep(Duration::from_millis(10));
        }
        if !thread.is_finished() {
            warning!("ipc thread did not stop; leaving it");
        } else if thread.join().is_err() {
            warning!("ipc thread panicked");
        }
    }
}
