//! Small Win32 helpers: error context, owned handles, COM apartment, timer.

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_EVENT, WAIT_OBJECT_0};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateEventW, CreateWaitableTimerExW, INFINITE,
    SetEvent, SetWaitableTimer, TIMER_ALL_ACCESS, WaitForMultipleObjects,
};
use windows::core::PCWSTR;

use crate::VideoError;

pub(crate) trait Context<T> {
    fn ctx(self, context: &'static str) -> Result<T, VideoError>;
}

impl<T> Context<T> for windows::core::Result<T> {
    fn ctx(self, context: &'static str) -> Result<T, VideoError> {
        self.map_err(|e| win_err(context, &e))
    }
}

pub(crate) fn win_err(context: &'static str, e: &windows::core::Error) -> VideoError {
    VideoError::Windows {
        context,
        code: e.code().0 as u32,
        message: e.message().trim().to_string(),
    }
}

/// A manual-reset Win32 event, closed on drop.
#[derive(Debug)]
pub(crate) struct Event(HANDLE);

// SAFETY: a kernel event handle may be used from any thread.
unsafe impl Send for Event {}
// SAFETY: SetEvent/WaitFor* on the same handle are thread-safe.
unsafe impl Sync for Event {}

impl Event {
    pub(crate) fn new() -> Result<Self, VideoError> {
        // SAFETY: no security attributes or name; the returned handle is owned.
        let handle =
            unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.ctx("CreateEventW")?;
        Ok(Self(handle))
    }

    pub(crate) fn set(&self) {
        // SAFETY: the handle is a valid event for the lifetime of `self`.
        let _ = unsafe { SetEvent(self.0) };
    }

    pub(crate) fn handle(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: we own the handle and close it exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// A high-resolution one-shot waitable timer.
#[derive(Debug)]
pub(crate) struct Timer(HANDLE);

impl Timer {
    pub(crate) fn new() -> Result<Self, VideoError> {
        // SAFETY: unnamed timer, default security; the handle is owned.
        let handle = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
        }
        .ctx("CreateWaitableTimerExW(HIGH_RESOLUTION)")?;
        Ok(Self(handle))
    }

    /// Arms the timer to fire once after `hns` 100-ns ticks.
    pub(crate) fn arm_relative(&self, hns: i64) -> Result<(), VideoError> {
        // Negative due time = relative.
        let due = -hns.max(1);
        // SAFETY: valid timer handle; `due` outlives the call; no APC routine.
        unsafe { SetWaitableTimer(self.0, &due, 0, None, None, false) }.ctx("SetWaitableTimer")
    }

    pub(crate) fn handle(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        // SAFETY: we own the handle and close it exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Waits until one of `handles` is signalled; returns its index.
pub(crate) fn wait_any(handles: &[HANDLE]) -> Result<usize, VideoError> {
    // SAFETY: all handles are valid waitable handles owned by the caller.
    let r: WAIT_EVENT = unsafe { WaitForMultipleObjects(handles, false, INFINITE) };
    let index = r.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
    if index < handles.len() {
        Ok(index)
    } else {
        Err(VideoError::Other(format!(
            "WaitForMultipleObjects failed: 0x{:08X}",
            r.0
        )))
    }
}

/// Initialises COM (MTA) for the current thread and uninitialises on drop.
#[derive(Debug)]
pub(crate) struct ComApartment(std::marker::PhantomData<*const ()>);

impl ComApartment {
    pub(crate) fn init_mta() -> Result<Self, VideoError> {
        // SAFETY: plain COM initialisation for this thread.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        hr.ok().ctx("CoInitializeEx(MTA)")?;
        Ok(Self(std::marker::PhantomData))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: balanced with the successful CoInitializeEx in `init_mta`
        // on this same thread (the guard is !Send).
        unsafe { CoUninitialize() };
    }
}

/// UTF-16 buffer (possibly NUL-terminated) to `String`.
pub(crate) fn from_wide(s: &[u16]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}
