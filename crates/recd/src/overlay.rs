//! The quick panel's window (Alt+F7 by default): a topmost popup drawn with
//! Direct2D/DirectWrite into a DirectComposition surface, excluded from
//! capture so it never shows up in clips. What it shows and how it reacts
//! lives in [`crate::panel`].
//!
//! - **It takes focus while open.** Without injecting into the game or a
//!   keyboard hook (both ruled out, see docs/decisions.md D7 and D8), the
//!   only way to get the panel's keys without the game also acting on them
//!   is to be the foreground window. The NVIDIA, AMD and Xbox Game Bar
//!   overlays do the same; games see an ordinary focus change. Closing
//!   gives focus back to the window that had it, and clicking anywhere
//!   else closes the panel.
//! - **Not over exclusive fullscreen.** Nothing can be drawn there, and
//!   taking focus would minimize the game; `app` checks before opening.
//! - **Nothing stays allocated while closed.** The device, composition
//!   tree and window are created on open (~30 ms) and released on close.
//! - **Drawn by WARP, Direct3D's software rasterizer**, not the GPU: the
//!   panel's few rounded rectangles and lines of text take well under a
//!   millisecond on the CPU, its drawing never queues work on the GPU the
//!   game is using, and recd keeps ~6 MB after the first open instead of
//!   ~19 MB with a hardware device (measured, AMD). WARP's idle worker
//!   threads stay parked afterwards. The hardware device is the fallback.
//! - **DirectComposition rather than a GDI layered window** (as the toast
//!   uses): the panel is redrawn on every key press and hover, and needs
//!   antialiased text on a translucent card, which DirectWrite does
//!   properly and GDI does not.

use std::borrow::Cow;
use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1,
    D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_DRAW_TEXT_OPTIONS_CLIP,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_ROUNDED_RECT, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
    D2D1CreateFactory, ID2D1Device, ID2D1DeviceContext, ID2D1Factory1, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionSurface, IDCompositionTarget,
    IDCompositionVisual,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD,
    DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT,
    DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING,
    DWRITE_TRIMMING, DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_NO_WRAP,
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice, IDXGISurface};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint,
    ScreenToClient,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_SHIFT};
use windows::Win32::UI::Shell::{
    FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog,
    IShellItem, SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetForegroundWindow, HWND_TOPMOST, IDC_ARROW,
    LoadCursorW, RegisterClassW, SW_SHOW, SWP_NOACTIVATE, SetForegroundWindow,
    SetWindowDisplayAffinity, SetWindowPos, ShowWindow, WA_INACTIVE, WDA_EXCLUDEFROMCAPTURE,
    WM_ACTIVATE, WM_CLOSE, WM_KEYDOWN, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_SYSKEYDOWN,
    WNDCLASSW, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{Interface, PCWSTR, w};

use crate::log::warning;
use crate::panel::{
    self, Command, Item, Key, Layout, Panel, View, estimate_mb, format_seconds, quality_name, size,
};
use crate::win::{Hwnd, wide};

/// Input for the open panel, from its window procedure. Positions are
/// client-area pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    Key(Key),
    Move(i32, i32),
    Click(i32, i32),
    /// +1 for a notch up, -1 down.
    Wheel(i32, i32, i32),
    /// Another window was activated.
    Deactivated,
    /// Alt+F4 or the system menu.
    Close,
}

unsafe extern "system" fn panel_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let point = || {
        let l = lparam.0 as u32;
        (i32::from(l as i16), i32::from((l >> 16) as i16))
    };
    let input = match msg {
        WM_KEYDOWN => {
            // SAFETY: plain keyboard state query.
            let shift = unsafe { GetKeyState(i32::from(VK_SHIFT.0)) } < 0;
            Key::from_vk(wparam.0 as u16, shift).map(Input::Key)
        }
        WM_MOUSEMOVE => {
            let (x, y) = point();
            Some(Input::Move(x, y))
        }
        WM_LBUTTONDOWN => {
            let (x, y) = point();
            Some(Input::Click(x, y))
        }
        WM_MOUSEWHEEL => {
            // Wheel positions are in screen coordinates.
            let (x, y) = point();
            let mut p = POINT { x, y };
            // SAFETY: our own window; `p` is a valid in/out pointer.
            let _ = unsafe { ScreenToClient(hwnd, &mut p) };
            let delta = (wparam.0 >> 16) as u16 as i16;
            Some(Input::Wheel(p.x, p.y, i32::from(delta.signum())))
        }
        WM_ACTIVATE if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE => Some(Input::Deactivated),
        WM_CLOSE => Some(Input::Close),
        // Alt+F4 arrives as WM_SYSKEYDOWN and becomes WM_CLOSE in
        // DefWindowProc; other Alt chords are ignored.
        WM_SYSKEYDOWN => None,
        _ => None,
    };
    match input {
        Some(input) => {
            crate::app::post(crate::app::Event::Panel(input));
            LRESULT(0)
        }
        // SAFETY: forwarding the original arguments.
        None => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// The open quick panel. Dropping it closes the window.
#[derive(Debug)]
pub struct Overlay {
    hwnd: Hwnd,
    /// The foreground window before the panel opened, given focus back on
    /// close.
    previous: Hwnd,
    monitor: HMONITOR,
    /// Device pixels per DIP.
    scale: f32,
    panel: Panel,
    view: View,
    layout: Layout,
    text: Text,
    gfx: Option<Graphics>,
    /// The size the window and surface were made for, in pixels.
    pixels: (u32, u32),
}

impl Overlay {
    /// Opens the panel centered on `monitor` (primary if `None`), draws
    /// `view` and takes focus.
    pub fn open(view: View, monitor: Option<HMONITOR>) -> windows::core::Result<Self> {
        // SAFETY: plain queries; MonitorFromPoint never fails with
        // MONITOR_DEFAULTTOPRIMARY.
        let (previous, monitor) = unsafe {
            (
                GetForegroundWindow(),
                monitor.unwrap_or_else(|| {
                    MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY)
                }),
            )
        };
        let (mut dpi_x, mut dpi_y) = (96, 96);
        // SAFETY: valid monitor handle and out-pointers.
        let _ = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
        let scale = dpi_x as f32 / 96.0;
        let hwnd = create_window()?;
        let layout = Layout::new(&view);
        let mut overlay = Self {
            hwnd,
            previous: previous.into(),
            monitor,
            scale,
            panel: Panel::new(&view),
            view,
            layout,
            text: Text::new()?,
            gfx: None,
            pixels: (0, 0),
        };
        overlay.place();
        overlay.draw();
        // SAFETY: our own window. A process that just received a hotkey
        // (or a tray click) may take the foreground; if Windows refuses
        // anyway, the panel shows unfocused and a click focuses it.
        unsafe {
            let _ = ShowWindow(overlay.hwnd.get(), SW_SHOW);
            let _ = SetForegroundWindow(overlay.hwnd.get());
        }
        Ok(overlay)
    }

    /// Shows a changed view, keeping the selection on an enabled row.
    pub fn update(&mut self, view: View) {
        if view == self.view {
            return;
        }
        self.view = view;
        self.panel.fix_selection(&self.view, 1);
        let layout = Layout::new(&self.view);
        if layout != self.layout {
            self.layout = layout;
            self.place();
        }
        self.draw();
    }

    /// Handles input; returns what the user asked for.
    pub fn input(&mut self, input: Input) -> Option<Command> {
        let dip = |v: i32| v as f32 / self.scale;
        let before = self.panel.clone();
        let command = match input {
            Input::Key(key) => self.panel.key(&self.view, key),
            Input::Move(x, y) => {
                if let Some((row, _)) = self.layout.hit(dip(x), dip(y)) {
                    self.panel.hover(&self.view, row);
                }
                None
            }
            Input::Click(x, y) => self
                .layout
                .hit(dip(x), dip(y))
                .and_then(|(row, side)| self.panel.click(&self.view, row, side)),
            Input::Wheel(x, y, dir) => self
                .layout
                .hit(dip(x), dip(y))
                .and_then(|(row, _)| self.panel.wheel(&self.view, row, dir)),
            Input::Deactivated | Input::Close => Some(Command::Close),
        };
        if self.panel != before {
            self.draw();
        }
        command
    }

    /// The window the panel gives focus back to.
    pub fn previous(&self) -> Hwnd {
        self.previous
    }

    /// Closes the panel; gives focus back to the window that had it if
    /// `restore` and the panel still has it.
    pub fn close(self, restore: bool) {
        // SAFETY: plain queries and focus calls with possibly stale
        // handles, which Windows tolerates.
        unsafe {
            if restore && GetForegroundWindow() == self.hwnd.get() && self.previous.0 != 0 {
                let _ = SetForegroundWindow(self.previous.get());
            }
        }
        drop(self);
    }

    /// Sizes and centers the window on its monitor for the current layout.
    fn place(&mut self) {
        let width = (self.layout.width * self.scale).ceil() as i32;
        let height = (self.layout.height * self.scale).ceil() as i32;
        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: valid monitor handle and struct.
        let work = if unsafe { GetMonitorInfoW(self.monitor, &mut mi) }.as_bool() {
            mi.rcWork
        } else {
            RECT {
                right: width,
                bottom: height,
                ..Default::default()
            }
        };
        let x = work.left + (work.right - work.left - width) / 2;
        let y = work.top + ((work.bottom - work.top - height) / 2).max(0);
        // SAFETY: our own window.
        let _ = unsafe {
            SetWindowPos(
                self.hwnd.get(),
                Some(HWND_TOPMOST),
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE,
            )
        };
        let pixels = (width as u32, height as u32);
        if pixels != self.pixels {
            self.pixels = pixels;
            // The surface is sized for the window: make a new one.
            self.gfx = None;
        }
    }

    fn draw(&mut self) {
        if self.gfx.is_none() {
            match Graphics::new(self.hwnd, self.pixels) {
                Ok(gfx) => self.gfx = Some(gfx),
                Err(e) => {
                    warning!("quick panel: graphics unavailable: {e}");
                    return;
                }
            }
        }
        let Some(gfx) = &self.gfx else {
            return;
        };
        let scene = Scene {
            panel: &self.panel,
            view: &self.view,
            layout: &self.layout,
            text: &self.text,
        };
        if let Err(e) = gfx.render(self.scale, |dc, brush, origin| {
            scene.paint(dc, brush, origin)
        }) {
            // Device lost or reset: start over with a new device next time.
            warning!("quick panel: drawing failed: {e}");
            self.gfx = None;
        }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        // Composition objects go before the window they target.
        self.gfx = None;
        // SAFETY: destroying our own window on its thread.
        let _ = unsafe { DestroyWindow(self.hwnd.get()) };
    }
}

fn create_window() -> windows::core::Result<Hwnd> {
    // SAFETY: registers a class with a valid window procedure (again on
    // later opens, which fails harmlessly) and creates a hidden popup.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(panel_proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: w!("rec.panel"),
            ..Default::default()
        };
        RegisterClassW(&class);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOREDIRECTIONBITMAP,
            w!("rec.panel"),
            w!("rec quick panel"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        // Keep our own UI out of every capture, including our clips.
        // REC_DEBUG_CAPTURE_UI=1 makes it visible to screenshots for testing.
        if std::env::var_os("REC_DEBUG_CAPTURE_UI").is_none() {
            let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
        }
        Ok(hwnd.into())
    }
}

// ---- drawing -------------------------------------------------------------------

/// The GPU side: a D3D device, a Direct2D context on it, and a composition
/// surface shown in the window.
#[derive(Debug)]
struct Graphics {
    dc: ID2D1DeviceContext,
    brush: ID2D1SolidColorBrush,
    composition: IDCompositionDevice,
    surface: IDCompositionSurface,
    // Kept alive for the composition tree.
    _visual: IDCompositionVisual,
    _target: IDCompositionTarget,
    _d2d: ID2D1Device,
    _d3d: ID3D11Device,
}

impl Graphics {
    fn new(hwnd: Hwnd, (width, height): (u32, u32)) -> windows::core::Result<Self> {
        let d3d = d3d_device()?;
        let dxgi: IDXGIDevice = d3d.cast()?;
        // SAFETY: COM factory and device creation with valid arguments; the
        // window outlives the target (see `Overlay::drop`).
        unsafe {
            let factory: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let d2d = factory.CreateDevice(&dxgi)?;
            let dc = d2d.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            // Cleartype needs an opaque target; the card is translucent.
            dc.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let brush = dc.CreateSolidColorBrush(&rgba(0xFFFFFF, 1.0), None)?;

            let composition: IDCompositionDevice = DCompositionCreateDevice(&dxgi)?;
            let target = composition.CreateTargetForHwnd(hwnd.get(), true)?;
            let visual = composition.CreateVisual()?;
            let surface = composition.CreateSurface(
                width.max(1),
                height.max(1),
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_ALPHA_MODE_PREMULTIPLIED,
            )?;
            visual.SetContent(&surface)?;
            target.SetRoot(&visual)?;
            Ok(Self {
                dc,
                brush,
                composition,
                surface,
                _visual: visual,
                _target: target,
                _d2d: d2d,
                _d3d: d3d,
            })
        }
    }

    /// Redraws the whole surface: `paint` gets the context, a brush to
    /// recolor, and the surface's origin in DIPs.
    fn render(
        &self,
        scale: f32,
        paint: impl FnOnce(&ID2D1DeviceContext, &ID2D1SolidColorBrush, (f32, f32)),
    ) -> windows::core::Result<()> {
        let mut offset = POINT::default();
        // SAFETY: BeginDraw/EndDraw are paired on every path below; the
        // bitmap wraps the surface only while it is being drawn.
        unsafe {
            let surface: IDXGISurface = self.surface.BeginDraw(None, &mut offset)?;
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                // Drawing in DIPs: Direct2D scales to device pixels.
                dpiX: 96.0 * scale,
                dpiY: 96.0 * scale,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                colorContext: ManuallyDrop::new(None),
            };
            let bitmap = match self.dc.CreateBitmapFromDxgiSurface(&surface, Some(&props)) {
                Ok(bitmap) => bitmap,
                Err(e) => {
                    let _ = self.surface.EndDraw();
                    return Err(e);
                }
            };
            self.dc.SetTarget(&bitmap);
            self.dc.BeginDraw();
            self.dc.Clear(Some(&rgba(0, 0.0)));
            let origin = (offset.x as f32 / scale, offset.y as f32 / scale);
            paint(&self.dc, &self.brush, origin);
            let drawn = self.dc.EndDraw(None, None);
            self.dc.SetTarget(None);
            let ended = self.surface.EndDraw();
            drawn?;
            ended?;
            self.composition.Commit()
        }
    }
}

fn d3d_device() -> windows::core::Result<ID3D11Device> {
    let mut last = None;
    // Software first (see the module docs), the GPU if WARP is missing.
    for driver in [D3D_DRIVER_TYPE_WARP, D3D_DRIVER_TYPE_HARDWARE] {
        let mut device = None;
        // SAFETY: valid arguments; the out-pointer is valid for writes.
        let result = unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        };
        match (result, device) {
            (Ok(()), Some(device)) => return Ok(device),
            (Err(e), _) => last = Some(e),
            (Ok(()), None) => {}
        }
    }
    Err(last.unwrap_or_else(windows::core::Error::empty))
}

/// Text formats, created once per panel.
#[derive(Debug)]
struct Text {
    title: IDWriteTextFormat,
    status: IDWriteTextFormat,
    label: IDWriteTextFormat,
    value: IDWriteTextFormat,
    centered: IDWriteTextFormat,
    detail: IDWriteTextFormat,
    path: IDWriteTextFormat,
    hint: IDWriteTextFormat,
}

impl Text {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: COM factory calls with valid arguments.
        unsafe {
            let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let format = |size: f32, weight: DWRITE_FONT_WEIGHT, align: DWRITE_TEXT_ALIGNMENT| {
                let f = factory.CreateTextFormat(
                    w!("Segoe UI"),
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!(""),
                )?;
                f.SetTextAlignment(align)?;
                f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
                f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
                let ellipsis = factory.CreateEllipsisTrimmingSign(&f)?;
                let trimming = DWRITE_TRIMMING {
                    granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                    delimiter: 0,
                    delimiterCount: 0,
                };
                f.SetTrimming(&trimming, &ellipsis)?;
                windows::core::Result::Ok(f)
            };
            let (lead, trail) = (
                DWRITE_TEXT_ALIGNMENT_LEADING,
                DWRITE_TEXT_ALIGNMENT_TRAILING,
            );
            let path = format(13.0, DWRITE_FONT_WEIGHT_NORMAL, trail)?;
            // Long paths keep their last folder: "C:\…\Videos\rec".
            let ellipsis = factory.CreateEllipsisTrimmingSign(&path)?;
            let trimming = DWRITE_TRIMMING {
                granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                delimiter: u32::from('\\'),
                delimiterCount: 2,
            };
            path.SetTrimming(&trimming, &ellipsis)?;
            Ok(Self {
                title: format(15.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, lead)?,
                status: format(12.0, DWRITE_FONT_WEIGHT_NORMAL, lead)?,
                label: format(13.5, DWRITE_FONT_WEIGHT_NORMAL, lead)?,
                value: format(13.5, DWRITE_FONT_WEIGHT_SEMI_BOLD, trail)?,
                centered: format(
                    13.5,
                    DWRITE_FONT_WEIGHT_SEMI_BOLD,
                    DWRITE_TEXT_ALIGNMENT_CENTER,
                )?,
                detail: format(12.0, DWRITE_FONT_WEIGHT_NORMAL, trail)?,
                path,
                hint: format(
                    11.5,
                    DWRITE_FONT_WEIGHT_NORMAL,
                    DWRITE_TEXT_ALIGNMENT_CENTER,
                )?,
            })
        }
    }
}

/// A straight-alpha color from `0xRRGGBB`.
fn rgba(rgb: u32, a: f32) -> D2D1_COLOR_F {
    let c = |shift: u32| ((rgb >> shift) & 0xFF) as f32 / 255.0;
    D2D1_COLOR_F {
        r: c(16),
        g: c(8),
        b: c(0),
        a,
    }
}

mod color {
    pub const CARD: u32 = 0x1C1C1E;
    pub const BORDER: u32 = 0xFFFFFF;
    pub const TEXT: u32 = 0xF2F2F2;
    pub const DIM: u32 = 0x9A9A9E;
    pub const DISABLED: u32 = 0x5C5C60;
    pub const ACCENT: u32 = 0x42A5F5;
    pub const ON: u32 = 0x4CAF50;
    pub const OFF: u32 = 0x48484C;
    pub const ALERT: u32 = 0xFF7043;
}

/// One frame of the panel.
struct Scene<'a> {
    panel: &'a Panel,
    view: &'a View,
    layout: &'a Layout,
    text: &'a Text,
}

impl Scene<'_> {
    fn paint(&self, dc: &ID2D1DeviceContext, brush: &ID2D1SolidColorBrush, (ox, oy): (f32, f32)) {
        let rect = |x: f32, y: f32, w: f32, h: f32| D2D_RECT_F {
            left: ox + x,
            top: oy + y,
            right: ox + x + w,
            bottom: oy + y + h,
        };
        let set = |rgb: u32, a: f32| {
            // SAFETY: recoloring our own brush.
            unsafe { brush.SetColor(&rgba(rgb, a)) }
        };
        let fill = |r: D2D_RECT_F, radius: f32| {
            let rr = D2D1_ROUNDED_RECT {
                rect: r,
                radiusX: radius,
                radiusY: radius,
            };
            // SAFETY: drawing between BeginDraw and EndDraw.
            unsafe { dc.FillRoundedRectangle(&rr, brush) };
        };
        let text = |s: &str, format: &IDWriteTextFormat, r: D2D_RECT_F| {
            let mut utf16 = wide(s);
            utf16.pop();
            // SAFETY: drawing between BeginDraw and EndDraw.
            unsafe {
                dc.DrawText(
                    &utf16,
                    format,
                    &r,
                    brush,
                    D2D1_DRAW_TEXT_OPTIONS_CLIP,
                    DWRITE_MEASURING_MODE_NATURAL,
                )
            };
        };
        let (width, height) = (self.layout.width, self.layout.height);
        let pad = size::PAD;

        // Card, with a hairline edge so it reads on dark and light scenes.
        set(color::BORDER, 0.10);
        fill(rect(0.0, 0.0, width, height), size::RADIUS);
        // Opaque enough that busy game scenes don't show through the text.
        set(color::CARD, 0.98);
        fill(
            rect(1.0, 1.0, width - 2.0, height - 2.0),
            size::RADIUS - 1.0,
        );

        // Header: a state dot, the title and the status line.
        let dot = if self.view.alert {
            color::ALERT
        } else if self.view.recording {
            color::ON
        } else {
            color::DIM
        };
        set(dot, 1.0);
        fill(rect(pad, 21.0, 9.0, 9.0), 4.5);
        set(color::TEXT, 1.0);
        text(
            &self.view.title,
            &self.text.title,
            rect(pad + 17.0, 12.0, width - 2.0 * pad - 17.0, 22.0),
        );
        set(
            if self.view.alert {
                color::ALERT
            } else {
                color::DIM
            },
            1.0,
        );
        text(
            &self.view.status,
            &self.text.status,
            rect(pad, 36.0, width - 2.0 * pad, 18.0),
        );

        set(color::BORDER, 0.07);
        for &y in &self.layout.dividers {
            fill(
                rect(pad, y + size::GAP / 2.0 - 0.5, width - 2.0 * pad, 1.0),
                0.0,
            );
        }

        let selected = self.panel.selected(self.view);
        let value_x = width - pad - size::VALUE;
        for (item, &top) in Panel::items(self.view).iter().zip(&self.layout.rows) {
            let item = *item;
            let enabled = Panel::enabled(self.view, item);
            if Some(item) == selected {
                set(color::ACCENT, 0.20);
                fill(rect(8.0, top + 1.0, width - 16.0, size::ROW - 2.0), 7.0);
                set(color::ACCENT, 1.0);
                fill(rect(8.0, top + 9.0, 3.0, size::ROW - 18.0), 1.5);
            }
            let row = |x: f32, w: f32| rect(x, top, w, size::ROW);
            let label_w = value_x - pad - 8.0;
            set(
                if enabled {
                    color::TEXT
                } else {
                    color::DISABLED
                },
                1.0,
            );
            text(
                &label(self.view, item),
                &self.text.label,
                row(pad + 4.0, label_w),
            );

            // Small grey detail at the right end of the label column.
            if let Some((detail, alert)) = detail(self.view, item) {
                set(if alert { color::ALERT } else { color::DIM }, 1.0);
                text(&detail, &self.text.detail, row(pad, label_w));
            }

            match item {
                Item::Profile | Item::Length | Item::Quality => {
                    let own = self.view.profile.as_ref().filter(|p| p.own);
                    let value = match item {
                        Item::Profile => own.map_or("All games", |p| &p.game).to_string(),
                        Item::Length => format_seconds(self.view.seconds),
                        _ => quality_name(self.view.quality).to_string(),
                    };
                    let (back, forward) = match item {
                        Item::Profile => (own.is_some(), own.is_none()),
                        Item::Length => (
                            panel::step_length(self.view.seconds, -1).is_some(),
                            panel::step_length(self.view.seconds, 1).is_some(),
                        ),
                        _ => (
                            panel::step_quality(self.view.quality, -1).is_some(),
                            panel::step_quality(self.view.quality, 1).is_some(),
                        ),
                    };
                    let arrow = 22.0;
                    set(color::TEXT, if back { 0.9 } else { 0.2 });
                    text("‹", &self.text.centered, row(value_x, arrow));
                    set(color::TEXT, if forward { 0.9 } else { 0.2 });
                    text("›", &self.text.centered, row(width - pad - arrow, arrow));
                    set(color::TEXT, 1.0);
                    text(
                        &value,
                        &self.text.centered,
                        row(value_x + arrow, size::VALUE - 2.0 * arrow),
                    );
                }
                Item::Audio(i) => {
                    let on = self.view.audio.get(i).is_some_and(|a| a.on);
                    let (w, h) = (34.0, 18.0);
                    let x = width - pad - w;
                    let y = top + (size::ROW - h) / 2.0;
                    set(if on { color::ON } else { color::OFF }, 1.0);
                    fill(rect(x, y, w, h), h / 2.0);
                    set(0xFFFFFF, 1.0);
                    let knob = h - 6.0;
                    let kx = if on { x + w - 3.0 - knob } else { x + 3.0 };
                    fill(rect(kx, y + 3.0, knob, knob), knob / 2.0);
                }
                Item::Save => {
                    set(
                        if enabled {
                            color::TEXT
                        } else {
                            color::DISABLED
                        },
                        1.0,
                    );
                    let value = format!("Last {}", format_seconds(self.view.seconds));
                    text(&value, &self.text.value, row(value_x, size::VALUE));
                }
                Item::SaveExtra(i) => {
                    // Its hotkey (and combo): the way to do this mid-fight.
                    let keys = self.view.extra_clips.get(i).map_or("", |c| &c.keys);
                    set(if enabled { color::DIM } else { color::DISABLED }, 1.0);
                    text(keys, &self.text.detail, row(value_x, size::VALUE));
                }
                Item::Star => {
                    let value = match &self.view.last_clip {
                        None => "No clip saved yet",
                        Some(c) if c.starred => "Starred",
                        Some(c) => &c.name,
                    };
                    set(
                        if enabled {
                            color::TEXT
                        } else {
                            color::DISABLED
                        },
                        1.0,
                    );
                    text(value, &self.text.path, row(value_x, size::VALUE));
                }
                Item::Folder => {
                    set(color::TEXT, 0.85);
                    text(
                        &self.view.folder,
                        &self.text.path,
                        row(value_x, size::VALUE),
                    );
                }
                Item::OpenFolder | Item::Pause => {}
            }
        }

        set(color::DIM, 1.0);
        let hint = format!(
            "↑↓ choose   ←→ change   Enter select   Esc close   ·   {} saves",
            self.view.save_hotkey
        );
        text(
            &hint,
            &self.text.hint,
            rect(pad, self.layout.footer, width - 2.0 * pad, size::FOOTER),
        );
    }
}

fn label(view: &View, item: Item) -> Cow<'static, str> {
    Cow::Borrowed(match item {
        Item::SaveExtra(i) => {
            let seconds = view.extra_clips.get(i).map_or(0, |c| c.seconds);
            return Cow::Owned(format!("Save last {}", format_seconds(seconds)));
        }
        Item::Save => "Save clip",
        Item::Star => "Star last clip",
        Item::Profile => "Settings for",
        Item::Length => "Clip length",
        Item::Quality => "Quality",
        Item::Audio(i) => view.audio.get(i).map_or("", |a| a.name),
        Item::Folder => "Save clips to",
        Item::OpenFolder => "Open clips folder",
        Item::Pause if view.paused => "Resume recording",
        Item::Pause => "Pause recording",
    })
}

/// A row's grey detail, and whether it is a warning.
fn detail(view: &View, item: Item) -> Option<(String, bool)> {
    match item {
        Item::Length => {
            let kbps = view.bitrate_kbps?;
            let mb = estimate_mb(view.seconds, kbps);
            let over = mb > u64::from(view.memory_cap_mb);
            let text = if over {
                format!("~{mb} MB, over the {} MB cap", view.memory_cap_mb)
            } else {
                format!("~{mb} MB")
            };
            Some((text, over))
        }
        Item::Quality => view
            .bitrate_kbps
            .map(|kbps| (format!("{} Mbit/s", panel::mbit(kbps)), false)),
        Item::Audio(i) => view
            .audio
            .get(i)
            .and_then(|a| a.note)
            .map(|n| (n.to_string(), false)),
        _ => None,
    }
}

// ---- folder picker ---------------------------------------------------------------

static PICKING: AtomicBool = AtomicBool::new(false);

/// Shows the standard folder picker on its own thread (its modal loop must
/// not stall recd's), then gives focus back to `restore` and calls `done`
/// with the chosen folder (`None` if cancelled). Does nothing while a
/// picker is already open.
pub fn pick_folder(
    initial: PathBuf,
    restore: Hwnd,
    done: impl FnOnce(Option<PathBuf>) + Send + 'static,
) {
    if PICKING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("rec-folder-picker".into())
        .spawn(move || {
            let chosen = show_picker(&initial)
                .map_err(|e| {
                    // Cancelling is an error too (ERROR_CANCELLED).
                    if e.code() != windows::Win32::Foundation::ERROR_CANCELLED.to_hresult() {
                        warning!("folder picker failed: {e}");
                    }
                })
                .ok();
            if restore.0 != 0 {
                // SAFETY: focus call with a possibly stale handle. This
                // thread owned the foreground dialog, so it may.
                let _ = unsafe { SetForegroundWindow(restore.get()) };
            }
            PICKING.store(false, Ordering::Release);
            done(chosen);
        });
    if spawned.is_err() {
        PICKING.store(false, Ordering::Release);
    }
}

fn show_picker(initial: &std::path::Path) -> windows::core::Result<PathBuf> {
    // SAFETY: COM is initialised for this thread for the whole function;
    // every interface is released before CoUninitialize; the returned
    // string is freed with CoTaskMemFree after copying.
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let result = (|| {
            let dialog: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
            dialog.SetOptions(
                dialog.GetOptions()? | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST,
            )?;
            dialog.SetTitle(w!("Save clips to"))?;
            let start = wide(initial);
            if let Ok(folder) =
                SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(start.as_ptr()), None)
            {
                let _ = dialog.SetFolder(&folder);
            }
            dialog.Show(None)?;
            let item = dialog.GetResult()?;
            let name = item.GetDisplayName(SIGDN_FILESYSPATH)?;
            let path = name.to_string();
            CoTaskMemFree(Some(name.0 as *const _));
            Ok(PathBuf::from(
                path.map_err(|_| windows::core::Error::empty())?,
            ))
        })();
        CoUninitialize();
        result
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use rec_core::config::Quality;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
    };

    use super::*;
    use crate::panel::{AudioRow, AudioTarget, ExtraClip, ProfileScope, QualityChoice};
    use crate::session::AudioSwitch;

    /// Shows the panel with extra clip rows for `REC_TEST_SECONDS` (default
    /// 5), visible to screenshots, without running recd. "Settings for"
    /// shows the game with `REC_TEST_PROFILE` set, else "All games".
    /// `cargo test -p recd shows_the_panel -- --ignored`
    #[test]
    #[ignore = "opens the panel, which takes focus"]
    fn shows_the_panel() {
        // SAFETY: set before anything in this test reads it.
        unsafe { std::env::set_var("REC_DEBUG_CAPTURE_UI", "1") };
        let seconds = std::env::var("REC_TEST_SECONDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);
        let view = View {
            title: "Recording fake_game".into(),
            status: "1 min buffered · HEVC 1080p60".into(),
            alert: false,
            recording: true,
            paused: false,
            profile: Some(ProfileScope {
                game: "fake_game".into(),
                own: std::env::var_os("REC_TEST_PROFILE").is_some(),
            }),
            seconds: 60,
            quality: QualityChoice::Preset(Quality::Balanced),
            bitrate_kbps: Some(18_000),
            memory_cap_mb: 1024,
            audio: vec![AudioRow {
                target: AudioTarget::Switch(AudioSwitch::Game),
                name: "Game",
                on: true,
                note: None,
            }],
            folder: r"C:\Users\me\Videos\rec".into(),
            save_hotkey: "Alt+F8".into(),
            extra_clips: vec![
                ExtraClip {
                    seconds: 15,
                    keys: "Alt+F6 · View+DPadDown".into(),
                },
                ExtraClip {
                    seconds: 300,
                    keys: "Alt+F5".into(),
                },
            ],
            last_clip: None,
        };
        let overlay = Overlay::open(view, None).unwrap();
        let until = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < until {
            let mut msg = MSG::default();
            // SAFETY: pumping this thread's own messages.
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        overlay.close(true);
    }
}
