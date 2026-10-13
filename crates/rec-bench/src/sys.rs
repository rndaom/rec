//! Thin, safe wrappers over the few Win32 calls the bench needs: process CPU
//! and memory counters, process lookup and waiting, the owner of a named pipe,
//! the token's elevation, the registry, the QPC frequency, the local date and
//! Ctrl+C handling.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, FILETIME, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
    STD_OUTPUT_HANDLE, SetConsoleCtrlHandler,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Performance::QueryPerformanceFrequency;
use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{
    GetActiveProcessorCount, GetCurrentProcess, GetProcessTimes, OpenProcess, OpenProcessToken,
    PROCESS_NAME_WIN32, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_VM_READ, QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows::core::{BOOL, HSTRING, PWSTR};

/// `GetActiveProcessorCount` argument meaning "every processor group".
const ALL_PROCESSOR_GROUPS: u16 = 0xFFFF;

/// Set by the Ctrl+C handler. Every wait in the bench polls it so an
/// interrupted run still restores `recd` before exiting.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn on_ctrl(kind: u32) -> BOOL {
    if kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT {
        INTERRUPTED.store(true, Ordering::SeqCst);
        // Handled: do not terminate us, we clean up and exit ourselves.
        true.into()
    } else {
        false.into()
    }
}

/// Makes Ctrl+C set a flag instead of killing the process.
pub fn install_ctrl_c_handler() {
    // SAFETY: `on_ctrl` is a valid handler for the lifetime of the process
    // and only touches an atomic.
    let _ = unsafe { SetConsoleCtrlHandler(Some(on_ctrl), true) };
}

/// Forgets a Ctrl+C so the cleanup that follows it (stopping and restoring
/// `recd`) can wait normally. The cleanup is bounded by timeouts.
pub fn clear_interrupt() {
    INTERRUPTED.store(false, Ordering::SeqCst);
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Sleeps for `duration`, returning early with an error if Ctrl+C arrives.
pub fn sleep(duration: Duration) -> Result<(), String> {
    let deadline = Instant::now() + duration;
    loop {
        if interrupted() {
            return Err("interrupted".into());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        std::thread::sleep(left.min(Duration::from_millis(100)));
    }
}

/// `QueryPerformanceFrequency`, in counts per second.
pub fn qpc_frequency() -> i64 {
    let mut frequency = 0;
    // SAFETY: the pointer is valid for writes; the call cannot fail on
    // Windows XP and later.
    let _ = unsafe { QueryPerformanceFrequency(&mut frequency) };
    frequency.max(1)
}

/// Logical processors across all processor groups.
pub fn logical_cores() -> u32 {
    // SAFETY: no pointers involved.
    unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) }.max(1)
}

/// Today's local date as `YYYY-MM-DD`.
pub fn local_date() -> String {
    // SAFETY: no pointers involved.
    let t = unsafe { GetLocalTime() };
    format!("{:04}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay)
}

/// Reads a `REG_SZ` value from `HKLM`.
pub fn registry_string(subkey: &str, value: &str) -> Option<String> {
    let subkey = HSTRING::from(subkey);
    let value = HSTRING::from(value);
    let mut size = 0u32;
    // SAFETY: the first call only asks for the required size; the strings
    // outlive the calls.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &subkey,
            &value,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    if status.is_err() || size == 0 {
        return None;
    }
    let mut buf = vec![0u16; (size as usize).div_ceil(2)];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: `buf` is `size` bytes long and writable.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &subkey,
            &value,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if status.is_err() {
        return None;
    }
    // `size` includes the terminating NUL.
    let chars = (size as usize / 2).min(buf.len());
    let text = String::from_utf16_lossy(&buf[..chars]);
    Some(text.trim_end_matches('\0').trim().to_string())
}

/// Reads a `REG_DWORD` value from `HKLM`.
pub fn registry_dword(subkey: &str, value: &str) -> Option<u32> {
    let subkey = HSTRING::from(subkey);
    let value = HSTRING::from(value);
    let mut data = 0u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: `data` is a writable `u32` and `size` says so.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &subkey,
            &value,
            RRF_RT_REG_DWORD,
            None,
            Some((&mut data as *mut u32).cast()),
            Some(&mut size),
        )
    };
    status.is_ok().then_some(data)
}

/// Process ids whose executable is named `exe_name` (case-insensitive).
pub fn find_processes(exe_name: &str) -> Vec<u32> {
    let mut pids = Vec::new();
    // SAFETY: plain snapshot of the process list; the handle is closed below.
    let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return pids;
    };
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // SAFETY: `entry.dwSize` is set and the snapshot handle is valid.
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok();
    while more {
        let len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
        if name.eq_ignore_ascii_case(exe_name) {
            pids.push(entry.th32ProcessID);
        }
        // SAFETY: as above.
        more = unsafe { Process32NextW(snapshot, &mut entry) }.is_ok();
    }
    // SAFETY: we own the snapshot handle.
    let _ = unsafe { CloseHandle(snapshot) };
    pids
}

/// Windows `ERROR_PIPE_BUSY`: every pipe instance is serving another client.
const ERROR_PIPE_BUSY: i32 = 231;

/// Id of the process serving the named pipe `name`, or `None` if there is no
/// such pipe.
///
/// This is how the bench knows which `recd` it is talking to: the process
/// name alone is ambiguous if more than one `recd.exe` exists. The client
/// handle is opened and closed without sending anything, so the server's read
/// ends at once instead of waiting for a request.
pub fn pipe_server_pid(name: &str) -> Result<Option<u32>, String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let pipe = loop {
        match OpenOptions::new().read(true).write(true).open(name) {
            Ok(pipe) => break pipe,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("could not open {name}: {e}")),
        }
    };
    let mut pid = 0u32;
    // SAFETY: the handle stays valid until `pipe` is dropped at the end of
    // this function and `pid` is valid for writes.
    unsafe { GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut pid) }
        .map_err(|e| format!("GetNamedPipeServerProcessId failed: {e}"))?;
    Ok(Some(pid))
}

/// Whether this process runs with an elevated (administrator) token. `false`
/// if that cannot be determined.
pub fn is_elevated() -> bool {
    let mut token = HANDLE::default();
    // SAFETY: the pseudo-handle from `GetCurrentProcess` needs no closing and
    // `token` is valid for writes.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.is_err() {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0u32;
    // SAFETY: `elevation` is a writable `TOKEN_ELEVATION` of the size passed
    // and `token` is a token handle opened for query.
    let queried = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    // SAFETY: we own the token handle and close it once.
    let _ = unsafe { CloseHandle(token) };
    queried.is_ok() && elevation.TokenIsElevated != 0
}

/// An open handle to another process, for reading its CPU time and memory.
#[derive(Debug)]
pub struct Process(HANDLE);

// SAFETY: a process handle is a kernel object reference that any thread may
// use; the sampler thread owns its handle exclusively.
unsafe impl Send for Process {}

impl Process {
    pub fn open(pid: u32) -> Result<Self, String> {
        // `GetProcessMemoryInfo` documents PROCESS_QUERY_INFORMATION | VM_READ;
        // the limited right also works on modern Windows and is the fallback
        // for processes that refuse the full one.
        // SAFETY: no pointers involved; the handle is closed in `Drop`.
        let handle = unsafe {
            OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid)
                .or_else(|_| OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid))
        };
        handle
            .map(Self)
            .map_err(|e| format!("could not open process {pid}: {e}"))
    }

    /// Opens a process only to wait for it to exit.
    pub fn open_waitable(pid: u32) -> Result<Self, String> {
        // SAFETY: no pointers involved; the handle is closed in `Drop`.
        unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                pid,
            )
        }
        .map(Self)
        .map_err(|e| format!("could not open process {pid}: {e}"))
    }

    /// Waits up to `timeout` for the process to exit. `Ok(true)` if it has.
    /// Returns an error early if Ctrl+C arrives.
    pub fn wait_exit(&self, timeout: Duration) -> Result<bool, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let slice = left.min(Duration::from_millis(100));
            // SAFETY: valid handle with the SYNCHRONIZE right.
            let result = unsafe { WaitForSingleObject(self.0, slice.as_millis() as u32) };
            if result == WAIT_OBJECT_0 {
                return Ok(true);
            }
            if result != WAIT_TIMEOUT {
                return Err("waiting for the process failed".into());
            }
            if left <= slice {
                return Ok(false);
            }
            if interrupted() {
                return Err("interrupted".into());
            }
        }
    }

    /// Kernel plus user CPU time consumed so far, in seconds.
    pub fn cpu_seconds(&self) -> Result<f64, String> {
        let (mut created, mut exited, mut kernel, mut user) = Default::default();
        // SAFETY: valid handle; every out-pointer is valid for writes.
        unsafe { GetProcessTimes(self.0, &mut created, &mut exited, &mut kernel, &mut user) }
            .map_err(|e| format!("GetProcessTimes failed: {e}"))?;
        let seconds = |t: FILETIME| {
            ((u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)) as f64 / 1e7
        };
        Ok(seconds(kernel) + seconds(user))
    }

    /// Private bytes (committed memory not shared with other processes),
    /// the number Task Manager calls "Commit size".
    pub fn private_bytes(&self) -> Result<u64, String> {
        let mut counters = PROCESS_MEMORY_COUNTERS_EX {
            cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        };
        // SAFETY: `counters` is `cb` bytes and the EX layout extends the base
        // struct this API takes a pointer to.
        unsafe {
            GetProcessMemoryInfo(
                self.0,
                (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX)
                    .cast::<PROCESS_MEMORY_COUNTERS>(),
                counters.cb,
            )
        }
        .map_err(|e| format!("GetProcessMemoryInfo failed: {e}"))?;
        Ok(counters.PrivateUsage as u64)
    }

    /// Full path of the process's executable.
    pub fn image_path(&self) -> Option<PathBuf> {
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        // SAFETY: `buf` holds `len` UTF-16 units and `len` is updated to the
        // number written.
        unsafe {
            QueryFullProcessImageNameW(
                self.0,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut len,
            )
        }
        .ok()?;
        Some(PathBuf::from(OsString::from_wide(&buf[..len as usize])))
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: we own the handle and close it once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Stops child processes from inheriting this process's stdin, stdout and
/// stderr.
///
/// `std::process::Command` creates children with handle inheritance on, so a
/// long-lived child (the `recd` the bench restarts for the user) would
/// otherwise keep the bench's output pipe open. `rec-bench ... | tee log`
/// would then never see end-of-file. The bench's own use of the handles is
/// unaffected; children get explicit `Stdio` settings instead.
pub fn make_std_handles_non_inheritable() {
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: querying a standard handle has no preconditions.
        let Ok(handle) = (unsafe { GetStdHandle(which) }) else {
            continue;
        };
        if handle.is_invalid() {
            continue;
        }
        // SAFETY: valid handle; only the inherit flag is changed. Failure
        // (e.g. for a pseudo-console handle) is harmless.
        let _ = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn a_missing_pipe_has_no_server() {
        assert_eq!(
            pipe_server_pid(r"\.\pipe\rec-bench-test-no-such-pipe").unwrap(),
            None
        );
    }

    #[test]
    fn elevation_can_be_queried() {
        // Either answer is valid; the call must not crash.
        let _ = is_elevated();
    }

    #[test]
    fn waiting_for_a_process_sees_it_exit() {
        let mut child = Command::new("cmd").args(["/C", "exit"]).spawn().unwrap();
        let watch = Process::open_waitable(child.id()).unwrap();
        assert!(watch.wait_exit(Duration::from_secs(10)).unwrap());
        let _ = child.wait();

        let mut long = Command::new("cmd")
            .args(["/C", "ping -n 20 127.0.0.1 >nul"])
            .spawn()
            .unwrap();
        let watch = Process::open_waitable(long.id()).unwrap();
        assert!(!watch.wait_exit(Duration::from_millis(200)).unwrap());
        let _ = long.kill();
        let _ = long.wait();
        assert!(watch.wait_exit(Duration::from_secs(10)).unwrap());
    }
}
