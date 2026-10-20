//! Explorer-side actions: the Recycle Bin, the clipboard, showing a file in
//! its folder and opening it in the default player.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_HDROP;
use windows::Win32::UI::Shell::{
    DROPFILES, FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
    FOF_WANTNUKEWARNING, SHFILEOPSTRUCTW, SHFileOperationW, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Paths as one buffer of NUL-terminated strings ending in an extra NUL, the
/// list format of `SHFileOperationW` and `CF_HDROP`.
fn path_list(paths: &[PathBuf]) -> Vec<u16> {
    let mut list = Vec::new();
    for p in paths {
        let absolute = std::path::absolute(p).unwrap_or_else(|_| p.clone());
        list.extend(wide(absolute.as_os_str()));
    }
    list.push(0);
    list
}

/// Moves files and folders to the Recycle Bin. Where the drive has no
/// Recycle Bin, Windows asks before deleting for good.
pub fn recycle(owner: Option<HWND>, paths: &[PathBuf]) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    let from = path_list(paths);
    let mut op = SHFILEOPSTRUCTW {
        hwnd: owner.unwrap_or_default(),
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO
            | FOF_NOCONFIRMATION
            | FOF_SILENT
            | FOF_WANTNUKEWARNING
            | FOF_NOERRORUI)
            .0 as u16,
        ..Default::default()
    };
    // SAFETY: `from` is a double-NUL-terminated list that outlives the call.
    let result = unsafe { SHFileOperationW(&mut op) };
    if result != 0 {
        return Err(format!(
            "couldn't move to the Recycle Bin (error {result:#x}); is the clip open in a player?"
        ));
    }
    if op.fAnyOperationsAborted.as_bool() {
        return Err("cancelled".into());
    }
    Ok(())
}

/// Puts files on the clipboard as Explorer does, so they can be pasted
/// into Explorer, Discord, a browser or a chat app.
pub fn copy_files(owner: HWND, paths: &[PathBuf]) -> Result<(), String> {
    let list = path_list(paths);
    let header = size_of::<DROPFILES>();
    let bytes = header + list.len() * 2;
    // SAFETY: the clipboard is opened with our window as owner and always
    // closed; the HGLOBALs are filled while locked and handed to the
    // clipboard, which then owns them (freed by us only on failure).
    unsafe {
        let drop = alloc(bytes, |mem| {
            let files = mem.cast::<DROPFILES>();
            (*files).pFiles = header as u32;
            (*files).fWide = true.into();
            std::ptr::copy_nonoverlapping(list.as_ptr(), mem.add(header).cast::<u16>(), list.len());
        })?;
        // DROPEFFECT_COPY: paste copies the clip rather than moving it.
        let effect = match alloc(4, |mem| mem.cast::<u32>().write_unaligned(1)) {
            Ok(effect) => effect,
            Err(e) => {
                let _ = GlobalFree(Some(drop));
                return Err(e);
            }
        };
        // Each block is the clipboard's once SetClipboardData takes it, and
        // ours to free otherwise.
        let mut ours = [Some(drop), Some(effect)];
        let result = OpenClipboard(Some(owner))
            .map_err(|e| format!("clipboard busy: {e}"))
            .and_then(|()| {
                let set = (|| {
                    EmptyClipboard().map_err(|e| e.to_string())?;
                    SetClipboardData(CF_HDROP.0.into(), Some(HANDLE(drop.0)))
                        .map_err(|e| e.to_string())?;
                    ours[0] = None;
                    let format = RegisterClipboardFormatW(w!("Preferred DropEffect"));
                    SetClipboardData(format, Some(HANDLE(effect.0))).map_err(|e| e.to_string())?;
                    ours[1] = None;
                    Ok::<(), String>(())
                })();
                let _ = CloseClipboard();
                set
            });
        for mem in ours.into_iter().flatten() {
            let _ = GlobalFree(Some(mem));
        }
        result.map_err(|e| format!("couldn't copy to the clipboard: {e}"))
    }
}

/// # Safety
/// `fill` must write at most `bytes` bytes.
unsafe fn alloc(bytes: usize, fill: impl FnOnce(*mut u8)) -> Result<HGLOBAL, String> {
    // SAFETY: a fresh moveable block, zeroed by the fill below or left as
    // allocated; locked only while it is filled.
    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE, bytes).map_err(|e| e.to_string())?;
        let ptr = GlobalLock(mem).cast::<u8>();
        if ptr.is_null() {
            let _ = GlobalFree(Some(mem));
            return Err("GlobalLock failed".into());
        }
        std::ptr::write_bytes(ptr, 0, bytes);
        fill(ptr);
        let _ = GlobalUnlock(mem);
        Ok(mem)
    }
}

/// Opens Explorer on the file's folder with the file selected.
pub fn reveal(path: &Path) -> Result<(), String> {
    let mut arg = std::ffi::OsString::from("/select,\"");
    arg.push(path.as_os_str());
    arg.push("\"");
    // `explorer.exe` exits with 1 even when it worked.
    std::process::Command::new("explorer.exe")
        .raw_arg(arg)
        .spawn()
        .map(drop)
        .map_err(|e| format!("can't open Explorer: {e}"))
}

/// Opens a file in its default app (the clip's player).
pub fn open(path: &Path) -> Result<(), String> {
    let file = wide(path.as_os_str());
    // SAFETY: NUL-terminated strings that outlive the call.
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(file.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    // Values above 32 mean success.
    if result.0 as isize > 32 {
        return Ok(());
    }
    // `.toml` often has no app: settings files open in Notepad then.
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("toml"))
    {
        let notepad = std::process::Command::new("notepad.exe").arg(path).spawn();
        if notepad.is_ok() {
            return Ok(());
        }
    }
    Err(format!("can't open {}", path.display()))
}
