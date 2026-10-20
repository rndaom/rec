//! rec-ui: settings, the clip library and the trimmer (decision D9).
//!
//! A separate process from recd that runs only while its window is open and
//! exits when it closes (D6). It reads and writes the same files recd does
//! and asks recd for status and a config reload over the IPC pipe; nothing
//! it does can stop or pause recording.

#![windows_subsystem = "windows"]

mod library;
mod recd;
mod settings;
mod shell;
mod worker;

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use tauri::{Manager, State, WebviewWindow};

use crate::library::Shared;
use crate::worker::Signal;

struct App {
    library: Arc<Shared>,
    signal: Arc<Signal>,
}

#[derive(Serialize)]
struct Info {
    version: &'static str,
    config_path: PathBuf,
    clips_dir: PathBuf,
    /// The picture under the cursor when dragging a clip that has no
    /// thumbnail yet.
    drag_icon: Option<PathBuf>,
}

#[tauri::command]
fn app_info() -> Info {
    Info {
        version: env!("CARGO_PKG_VERSION"),
        config_path: settings::config_path(),
        clips_dir: library::roots().0.remove(0),
        drag_icon: drag_icon(),
    }
}

/// The app icon as a file, for `tauri-plugin-drag`, which takes a path.
fn drag_icon() -> Option<PathBuf> {
    let path = rec_library::sys::data_dir().join("ui").join("drag.png");
    if !path.is_file() {
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::write(&path, include_bytes!("../icons/128x128.png")).ok()?;
    }
    Some(path)
}

#[tauri::command(async)]
fn recd_state() -> recd::RecdState {
    recd::state()
}

#[tauri::command(async)]
fn recd_start() -> Result<(), String> {
    recd::start()
}

#[tauri::command(async)]
fn settings_get(profile: Option<String>) -> settings::Snapshot {
    settings::snapshot(profile.as_deref())
}

#[tauri::command(async)]
fn settings_apply(
    profile: Option<String>,
    changes: Vec<settings::Change>,
) -> Result<settings::Saved, String> {
    settings::apply(profile.as_deref(), &changes)
}

#[tauri::command(async)]
fn library_list(app: State<'_, App>, window: WebviewWindow) -> library::Listing {
    let listing = library::list(&app.library);
    allow_folder(&window, &listing.folder);
    app.signal.poke();
    listing
}

#[tauri::command(async)]
fn clip_detail(app: State<'_, App>, key: String) -> Result<library::Detail, String> {
    library::detail(&app.library, &key)
}

#[tauri::command(async)]
fn clip_star(app: State<'_, App>, key: String, starred: bool) -> Result<(), String> {
    library::set_starred(&app.library, &key, starred)
}

/// Moves clips (video, sidecar and `<clip>.rec`) to the Recycle Bin.
#[tauri::command(async)]
fn clip_delete(app: State<'_, App>, window: WebviewWindow, keys: Vec<String>) -> Result<(), String> {
    let mut files = Vec::new();
    let mut thumbs = Vec::new();
    for key in &keys {
        let clip = app.library.clip(key)?;
        files.extend(library::files(&clip));
        thumbs.extend(clip.video.as_deref().map(library::cached_thumb));
    }
    shell::recycle(window.hwnd().ok(), &files)?;
    for t in thumbs {
        let _ = std::fs::remove_file(t);
    }
    Ok(())
}

#[tauri::command(async)]
fn clip_copy(window: WebviewWindow, keys: Vec<String>) -> Result<(), String> {
    let hwnd = window.hwnd().map_err(|e| e.to_string())?;
    let paths: Vec<PathBuf> = keys.iter().map(PathBuf::from).collect();
    shell::copy_files(hwnd, &paths)
}

#[tauri::command(async)]
fn reveal(path: PathBuf) -> Result<(), String> {
    shell::reveal(&path)
}

#[tauri::command(async)]
fn open_path(path: PathBuf) -> Result<(), String> {
    shell::open(&path)
}

/// Lets the webview load videos and thumbnails from a folder through the
/// asset protocol. Nothing else on disk is reachable from the page.
fn allow_folder(window: &WebviewWindow, folder: &std::path::Path) {
    let _ = window.asset_protocol_scope().allow_directory(folder, true);
}

fn main() {
    let shared = Arc::new(Shared::default());
    let signal = Arc::new(Signal::default());
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_drag::init())
        .manage(App {
            library: shared.clone(),
            signal: signal.clone(),
        })
        .setup(move |app| {
            let thumbs = library::cached_thumb(std::path::Path::new("x"));
            if let Some(dir) = thumbs.parent() {
                let _ = app.asset_protocol_scope().allow_directory(dir, false);
            }
            worker::spawn(app.handle().clone(), shared, signal);
            if let Some(window) = app.get_webview_window("main") {
                window.show()?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            recd_state,
            recd_start,
            settings_get,
            settings_apply,
            library_list,
            clip_detail,
            clip_star,
            clip_delete,
            clip_copy,
            reveal,
            open_path,
        ])
        .run(tauri::generate_context!())
        .expect("rec-ui failed to start");
}
