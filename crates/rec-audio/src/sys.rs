//! Small RAII wrappers around Win32 handles and per-thread state.

use windows::Win32::Foundation::{CloseHandle, HANDLE, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, SetEvent,
};
use windows::core::{IUnknown_Vtbl, Interface, w};

use crate::{AudioError, Context};

/// An auto-reset Win32 event.
#[derive(Debug)]
pub(crate) struct Event(HANDLE);

// SAFETY: an event handle is a kernel object reference that may be used from
// any thread; SetEvent / waits on it are thread-safe.
unsafe impl Send for Event {}
// SAFETY: see above; `&Event` only exposes thread-safe operations.
unsafe impl Sync for Event {}

impl Event {
    pub(crate) fn new() -> Result<Self, AudioError> {
        // SAFETY: no security attributes or name; the returned handle is owned
        // by the new `Event` and closed in `Drop`.
        let h = unsafe { CreateEventW(None, false, false, None) }.context("CreateEventW")?;
        Ok(Self(h))
    }

    pub(crate) fn handle(&self) -> HANDLE {
        self.0
    }

    pub(crate) fn set(&self) {
        // SAFETY: the handle is a valid event for the lifetime of `self`.
        // SetEvent can only fail for an invalid handle.
        let _ = unsafe { SetEvent(self.0) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: we own the handle and close it exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// COM initialised on the current thread (MTA) until dropped.
#[derive(Debug)]
pub(crate) struct ComGuard {
    uninit: bool,
}

impl ComGuard {
    pub(crate) fn mta() -> Result<Self, AudioError> {
        // SAFETY: plain per-thread COM initialisation; balanced in Drop.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            // The thread is already STA; COM is usable, just don't uninit.
            return Ok(Self { uninit: false });
        }
        hr.ok().context("CoInitializeEx")?;
        Ok(Self { uninit: true })
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.uninit {
            // SAFETY: balances the successful CoInitializeEx on this thread
            // (the guard is !Send, so it is dropped on the same thread).
            unsafe { CoUninitialize() };
        }
    }
}

/// The current thread registered with MMCSS as "Audio" until dropped.
#[derive(Debug)]
pub(crate) struct Mmcss(HANDLE);

impl Mmcss {
    /// Best effort: `None` if MMCSS is unavailable (the thread just runs at
    /// normal priority).
    pub(crate) fn audio() -> Option<Self> {
        let mut task_index = 0;
        // SAFETY: valid task name and out pointer; the handle is reverted in
        // Drop on this thread.
        unsafe { AvSetMmThreadCharacteristicsW(w!("Audio"), &mut task_index) }
            .ok()
            .map(Self)
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        // SAFETY: the handle came from AvSetMmThreadCharacteristicsW on this
        // thread (the guard is !Send).
        let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
    }
}

/// Current reference count of a COM object.
///
/// COM documents the value returned by `Release` as meaningful for tests only,
/// but Microsoft's own objects return the exact count. It is used to check
/// that the encoder no longer holds our reusable input sample; a wrong answer
/// can only make us allocate a fresh one.
pub(crate) fn ref_count<T: Interface>(obj: &T) -> u32 {
    let raw = obj.as_raw();
    // SAFETY: `raw` is a live COM object (we hold a reference through `obj`)
    // and every COM vtable starts with the IUnknown methods. A balanced
    // AddRef/Release leaves its count unchanged.
    unsafe {
        let vtable = &**(raw as *const *const IUnknown_Vtbl);
        (vtable.AddRef)(raw);
        (vtable.Release)(raw)
    }
}
