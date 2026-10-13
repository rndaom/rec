//! Process loopback support: OS check, finding the app to follow, noticing
//! when it exits, and activating the process loopback audio client.

use std::mem::ManuallyDrop;
use std::sync::Arc;

use windows::Win32::Foundation::{CloseHandle, HANDLE, STILL_ACTIVE};
use windows::Win32::Media::Audio::{
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioClient, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{BLOB, IAgileObject, IAgileObject_Impl};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{HRESULT, IUnknown, Interface, implement};

use crate::sys::Event;
use crate::{AudioError, Context, ProcessTarget};

/// First Windows build where process loopback works. Microsoft documents it
/// from build 20348, but it is present and used by other recorders from
/// Windows 10 2004 (19041) on.
const PROCESS_LOOPBACK_BUILD: u32 = 19041;

/// How long to wait for `ActivateAudioInterfaceAsync` (it took ~1.5 ms here).
const ACTIVATION_TIMEOUT_MS: u32 = 5_000;

/// The Windows build number, from `RtlGetVersion` (which, unlike
/// `GetVersionEx`, is not affected by the app manifest).
pub(crate) fn windows_build() -> u32 {
    #[repr(C)]
    struct OsVersionInfoW {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        csd_version: [u16; 128],
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlGetVersion(info: *mut OsVersionInfoW) -> i32;
    }
    let mut info = OsVersionInfoW {
        size: size_of::<OsVersionInfoW>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        csd_version: [0; 128],
    };
    // SAFETY: `info` is a correctly sized OSVERSIONINFOW with its size set.
    let status = unsafe { RtlGetVersion(&mut info) };
    if status == 0 { info.build } else { 0 }
}

/// Whether [`AudioSource::Process`](crate::AudioSource::Process) can work on
/// this system (Windows 10 2004 or later).
pub fn process_loopback_supported() -> bool {
    windows_build() >= PROCESS_LOOPBACK_BUILD
}

/// A running process we follow, held open with
/// `PROCESS_QUERY_LIMITED_INFORMATION` only (enough to see it exit, and it
/// keeps the pid from being reused while we hold it).
#[derive(Debug)]
pub(crate) struct Followed {
    pub(crate) pid: u32,
    pub(crate) exe: String,
    handle: HANDLE,
}

// SAFETY: a process handle may be used and closed from any thread.
unsafe impl Send for Followed {}

impl Followed {
    fn open(pid: u32, exe: String) -> Option<Self> {
        // SAFETY: plain OpenProcess; the handle is closed in Drop.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
        let followed = Self { pid, exe, handle };
        followed.alive().then_some(followed)
    }

    /// Still running. (A process that exits with code 259, `STILL_ACTIVE`,
    /// looks alive forever; telling them apart needs `SYNCHRONIZE` access,
    /// which we don't ask for. Its stream then just carries silence.)
    pub(crate) fn alive(&self) -> bool {
        let mut code = 0;
        // SAFETY: valid handle with PROCESS_QUERY_LIMITED_INFORMATION access.
        unsafe { GetExitCodeProcess(self.handle, &mut code) }.is_ok()
            && code == STILL_ACTIVE.0 as u32
    }

    pub(crate) fn describe(&self) -> String {
        format!("{} (pid {})", self.exe, self.pid)
    }
}

impl Drop for Followed {
    fn drop(&mut self) {
        // SAFETY: we own the handle and close it once.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

/// One entry of a process snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProcessEntry {
    pub(crate) pid: u32,
    pub(crate) parent: u32,
    pub(crate) exe: String,
}

fn snapshot() -> Vec<ProcessEntry> {
    let mut out = Vec::new();
    // SAFETY: the snapshot handle is closed below; the entry struct has its
    // size set as the API requires.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut ok = Process32FirstW(snap, &mut entry).is_ok();
        while ok {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            out.push(ProcessEntry {
                pid: entry.th32ProcessID,
                parent: entry.th32ParentProcessID,
                exe: String::from_utf16_lossy(&entry.szExeFile[..len]),
            });
            ok = Process32NextW(snap, &mut entry).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// Picks the process to follow among `processes`: the first executable in
/// `names` that runs, and of its instances the root of the tree (one whose
/// parent is not the same executable), so the include-tree loopback covers
/// every helper process (Discord plays audio from a child process).
pub(crate) fn pick_root(processes: &[ProcessEntry], names: &[String]) -> Option<ProcessEntry> {
    for name in names {
        let matching: Vec<&ProcessEntry> = processes
            .iter()
            .filter(|p| p.exe.eq_ignore_ascii_case(name))
            .collect();
        let is_match = |pid: u32| matching.iter().any(|p| p.pid == pid);
        if let Some(root) = matching
            .iter()
            .filter(|p| !is_match(p.parent))
            .min_by_key(|p| p.pid)
            .or_else(|| matching.iter().min_by_key(|p| p.pid))
        {
            return Some((*root).clone());
        }
    }
    None
}

/// Opens the process `target` currently refers to, if it is running.
pub(crate) fn resolve(target: &ProcessTarget) -> Option<Followed> {
    match target {
        ProcessTarget::Pid(pid) => {
            let exe = snapshot()
                .into_iter()
                .find(|p| p.pid == *pid)
                .map_or_else(|| "process".to_string(), |p| p.exe);
            Followed::open(*pid, exe)
        }
        ProcessTarget::Executables(names) => {
            let root = pick_root(&snapshot(), names)?;
            Followed::open(root.pid, root.exe)
        }
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Completion {
    done: Arc<Event>,
}

impl IAgileObject_Impl for Completion_Impl {}

impl IActivateAudioInterfaceCompletionHandler_Impl for Completion_Impl {
    fn ActivateCompleted(
        &self,
        _operation: windows::core::Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        self.done.set();
        Ok(())
    }
}

/// Activates an `IAudioClient` capturing what process `pid` and its children
/// play (or, with `exclude`, everything else). Blocks until activation
/// completes.
pub(crate) fn activate(pid: u32, exclude: bool) -> Result<IAudioClient, AudioError> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: if exclude {
                    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
                } else {
                    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE
                },
            },
        },
    };
    // A VT_BLOB PROPVARIANT pointing at `params`. It borrows our memory, so it
    // must never be cleared: windows-rs gives PROPVARIANT a Drop that calls
    // PropVariantClear, which would CoTaskMemFree `params` (heap corruption).
    let mut prop = ManuallyDrop::new(PROPVARIANT::default());
    // SAFETY: writing the active union members of a zeroed PROPVARIANT.
    unsafe {
        let inner = &mut *prop.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob = BLOB {
            cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: (&params as *const AUDIOCLIENT_ACTIVATION_PARAMS)
                .cast_mut()
                .cast(),
        };
    }
    let done = Arc::new(Event::new()?);
    let handler: IActivateAudioInterfaceCompletionHandler = Completion {
        done: Arc::clone(&done),
    }
    .into();
    // SAFETY: `prop` and `params` outlive the call (the API copies the
    // parameters before returning); the handler is a valid agile COM object.
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*prop),
            &handler,
        )
    }
    .context("ActivateAudioInterfaceAsync")?;
    // SAFETY: valid event handle owned by `done`.
    let wait = unsafe { WaitForSingleObject(done.handle(), ACTIVATION_TIMEOUT_MS) };
    if wait.0 != 0 {
        return Err(AudioError::Device(
            "process loopback activation timed out".into(),
        ));
    }
    let mut result = HRESULT(0);
    let mut client: Option<IUnknown> = None;
    // SAFETY: activation has completed (the handler ran); out pointers valid.
    unsafe { operation.GetActivateResult(&mut result, &mut client) }
        .context("GetActivateResult")?;
    result.ok().context("process loopback activation")?;
    client
        .ok_or_else(|| AudioError::Device("process loopback returned no client".into()))?
        .cast()
        .context("process loopback IAudioClient")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pid: u32, parent: u32, exe: &str) -> ProcessEntry {
        ProcessEntry {
            pid,
            parent,
            exe: exe.into(),
        }
    }

    #[test]
    fn picks_the_root_of_the_first_running_app() {
        let processes = [
            entry(4, 0, "System"),
            entry(900, 800, "Discord.exe"),
            entry(901, 900, "Discord.exe"),
            entry(800, 1, "explorer.exe"),
            entry(902, 900, "discord.exe"),
            entry(500, 800, "TeamSpeak.exe"),
        ];
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            pick_root(&processes, &names(&["Discord.exe", "TeamSpeak.exe"])).map(|p| p.pid),
            Some(900)
        );
        assert_eq!(
            pick_root(&processes, &names(&["Mumble.exe", "TeamSpeak.exe"])).map(|p| p.pid),
            Some(500)
        );
        assert_eq!(pick_root(&processes, &names(&["Mumble.exe"])), None);
    }

    #[test]
    fn a_cycle_of_same_named_processes_still_picks_one() {
        // Parent pids can be stale and reused, forming an apparent cycle.
        let processes = [entry(10, 11, "a.exe"), entry(11, 10, "a.exe")];
        assert_eq!(
            pick_root(&processes, &["a.exe".to_string()]).map(|p| p.pid),
            Some(10)
        );
    }

    #[test]
    fn this_process_resolves_and_is_alive() {
        let me = std::process::id();
        let followed = resolve(&ProcessTarget::Pid(me)).expect("own process");
        assert!(followed.alive());
        assert!(followed.describe().contains(&me.to_string()));
        assert!(process_loopback_supported() == (windows_build() >= 19041));
    }
}
