//! A stand-in game for testing capture edge cases by hand: a D3D11 window
//! that flips between two colours every second and can go fullscreen,
//! render black or stop presenting on cue. Once a second it prints what the
//! shell reports (`SHQueryUserNotificationState`).
//!
//! ```text
//! cargo run --release -p rec-video --example fake_game -- [--seconds N] \
//!     [--borderless | --fullscreen] [--blt] [--black-after S] [--stall-after S] //!     [--resume-after S] [--resize-after S]
//! ```
//!
//! `--borderless` covers the screen with a popup window (what games call
//! borderless or windowed fullscreen). `--fullscreen` also asks DXGI for
//! fullscreen (`SetFullscreenState`); `--blt`
//! uses the legacy blt-model swap chain instead of flip. `--resize-after`
//! grows the window (half the screen) to three quarters of the screen, like
//! a game switching resolution. Press Esc to quit.

#[cfg(not(windows))]
fn main() {
    eprintln!("this example only runs on Windows");
}

#[cfg(windows)]
fn main() {
    if let Err(e) = imp::main() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
mod imp {
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDeviceAndSwapChain,
        ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_DESC, DXGI_SAMPLE_DESC,
    };
    use windows::Win32::Graphics::Dxgi::{
        DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH,
        DXGI_SWAP_EFFECT_DISCARD, DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
        IDXGISwapChain,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
    use windows::Win32::UI::Shell::SHQueryUserNotificationState;
    use windows::Win32::UI::WindowsAndMessaging::{
        CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DispatchMessageW,
        GetSystemMetrics, MSG, PM_REMOVE, PeekMessageW, PostQuitMessage, RegisterClassW,
        SM_CXSCREEN, SM_CYSCREEN, SW_SHOW, SWP_NOMOVE, SWP_NOZORDER, SetForegroundWindow,
        SetWindowPos, ShowWindow, TranslateMessage, WM_DESTROY, WM_KEYDOWN, WM_QUIT, WNDCLASSW,
        WS_EX_APPWINDOW, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE,
    };
    use windows::core::w;

    struct Args {
        seconds: f64,
        fullscreen: bool,
        borderless: bool,
        blt: bool,
        black_after: Option<f64>,
        stall_after: Option<f64>,
        resume_after: Option<f64>,
        resize_after: Option<f64>,
    }

    fn parse_args() -> Result<Args, String> {
        let mut a = Args {
            seconds: 10.0,
            fullscreen: false,
            borderless: false,
            blt: false,
            black_after: None,
            stall_after: None,
            resume_after: None,
            resize_after: None,
        };
        let mut it = std::env::args().skip(1);
        let num = |v: Option<String>, name: &str| -> Result<f64, String> {
            v.and_then(|v| v.parse().ok())
                .ok_or(format!("{name} needs a number"))
        };
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--seconds" => a.seconds = num(it.next(), "--seconds")?,
                "--fullscreen" => a.fullscreen = true,
                "--borderless" => a.borderless = true,
                "--blt" => a.blt = true,
                "--black-after" => a.black_after = Some(num(it.next(), "--black-after")?),
                "--stall-after" => a.stall_after = Some(num(it.next(), "--stall-after")?),
                "--resume-after" => a.resume_after = Some(num(it.next(), "--resume-after")?),
                "--resize-after" => a.resize_after = Some(num(it.next(), "--resize-after")?),
                other => return Err(format!("unknown argument {other}")),
            }
        }
        Ok(a)
    }

    extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match msg {
            WM_DESTROY => {
                // SAFETY: called on the window's own thread.
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            WM_KEYDOWN if wparam.0 == usize::from(VK_ESCAPE.0) => {
                // SAFETY: as above.
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            // SAFETY: default handling of our own window's messages.
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    fn shell_state() -> String {
        // SAFETY: plain query.
        match unsafe { SHQueryUserNotificationState() } {
            Ok(s) => match s.0 {
                1 => "not present".into(),
                2 => "busy (fullscreen app)".into(),
                3 => "D3D fullscreen".into(),
                4 => "presentation mode".into(),
                5 => "accepts notifications".into(),
                6 => "quiet time".into(),
                7 => "app (Windows Store) fullscreen".into(),
                n => format!("state {n}"),
            },
            Err(e) => format!("error {e}"),
        }
    }

    pub fn main() -> Result<(), String> {
        let args = parse_args()?;
        // SAFETY: standard window class registration and creation; the
        // class name and title are static strings.
        let hwnd = unsafe {
            let instance: HINSTANCE = GetModuleHandleW(None).map_err(|e| e.to_string())?.into();
            let class = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                hInstance: instance,
                lpszClassName: w!("rec_fake_game"),
                ..Default::default()
            };
            RegisterClassW(&class);
            let (w, h) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
            let (style, w, h) = if args.fullscreen || args.borderless {
                (WS_POPUP | WS_VISIBLE, w, h)
            } else {
                (WS_OVERLAPPEDWINDOW | WS_VISIBLE, w / 2, h / 2)
            };
            CreateWindowExW(
                WS_EX_APPWINDOW,
                w!("rec_fake_game"),
                w!("rec fake game"),
                style,
                0,
                0,
                w,
                h,
                None,
                None,
                Some(instance),
                None,
            )
            .map_err(|e| e.to_string())?
        };
        // SAFETY: our own window.
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
        }

        let desc = DXGI_SWAP_CHAIN_DESC {
            BufferDesc: DXGI_MODE_DESC {
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                ..Default::default()
            },
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: if args.blt { 1 } else { 2 },
            OutputWindow: hwnd,
            Windowed: true.into(),
            SwapEffect: if args.blt {
                DXGI_SWAP_EFFECT_DISCARD
            } else {
                DXGI_SWAP_EFFECT_FLIP_DISCARD
            },
            Flags: DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH.0 as u32,
        };
        let mut swap: Option<IDXGISwapChain> = None;
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        // SAFETY: all out-pointers are valid locals.
        unsafe {
            D3D11CreateDeviceAndSwapChain(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&desc),
                Some(&mut swap),
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .map_err(|e| format!("D3D11CreateDeviceAndSwapChain: {e}"))?;
        let (swap, device, context) = (swap.unwrap(), device.unwrap(), context.unwrap());
        if args.fullscreen {
            // SAFETY: our own swap chain.
            unsafe { swap.SetFullscreenState(true, None) }
                .map_err(|e| format!("SetFullscreenState: {e}"))?;
            // SAFETY: as above; 0 keeps the buffer count and size follows
            // the new window size.
            unsafe {
                swap.ResizeBuffers(
                    0,
                    0,
                    0,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_SWAP_CHAIN_FLAG(desc.Flags as i32),
                )
            }
            .map_err(|e| format!("ResizeBuffers: {e}"))?;
        }
        let target = || -> Result<ID3D11RenderTargetView, String> {
            // SAFETY: buffer 0 of our own swap chain; the view out-pointer
            // is a valid local.
            unsafe {
                let buffer: ID3D11Texture2D = swap.GetBuffer(0).map_err(|e| e.to_string())?;
                let mut rtv = None;
                device
                    .CreateRenderTargetView(&buffer, None, Some(&mut rtv))
                    .map_err(|e| e.to_string())?;
                Ok(rtv.unwrap())
            }
        };
        let mut rtv = target()?;

        let start = Instant::now();
        let mut last_print = 0u64;
        let mut presents = 0u64;
        let mut resized = false;
        loop {
            let mut msg = MSG::default();
            // SAFETY: standard message pump on the window's thread.
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_QUIT {
                        return Ok(());
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            let t = start.elapsed().as_secs_f64();
            if t >= args.seconds {
                break;
            }
            let after = |s: Option<f64>| s.is_some_and(|s| t >= s);
            if after(args.resize_after) && !resized {
                resized = true;
                // SAFETY: our own window; the swap chain is resized below.
                unsafe {
                    let (w, h) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
                    SetWindowPos(
                        hwnd,
                        None,
                        0,
                        0,
                        w * 3 / 4,
                        h * 3 / 4,
                        SWP_NOMOVE | SWP_NOZORDER,
                    )
                    .map_err(|e| e.to_string())?;
                }
                drop(rtv);
                // SAFETY: as for ResizeBuffers above.
                unsafe {
                    swap.ResizeBuffers(
                        0,
                        0,
                        0,
                        DXGI_FORMAT_B8G8R8A8_UNORM,
                        DXGI_SWAP_CHAIN_FLAG(desc.Flags as i32),
                    )
                }
                .map_err(|e| format!("ResizeBuffers: {e}"))?;
                rtv = target()?;
                println!("resized");
            }
            let stalled = after(args.stall_after) && !after(args.resume_after);
            let black = after(args.black_after) && !after(args.resume_after);
            if t as u64 > last_print {
                last_print = t as u64;
                println!(
                    "t={:>2}s presents={presents:>5} black={black} stalled={stalled} shell: {}",
                    last_print,
                    shell_state()
                );
            }
            if stalled {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            let color = if black {
                [0.0, 0.0, 0.0, 1.0]
            } else if (t as u64) % 2 == 0 {
                [0.9, 0.2, 0.1, 1.0]
            } else {
                [0.1, 0.4, 0.9, 1.0]
            };
            // SAFETY: our own device objects on this thread.
            let hr = unsafe {
                context.ClearRenderTargetView(&rtv, &color);
                swap.Present(1, Default::default())
            };
            if hr.is_err() {
                println!("Present: {hr:?}");
                std::thread::sleep(Duration::from_millis(100));
                drop(rtv);
                // SAFETY: as for ResizeBuffers above.
                let _ = unsafe {
                    swap.ResizeBuffers(
                        0,
                        0,
                        0,
                        DXGI_FORMAT_B8G8R8A8_UNORM,
                        DXGI_SWAP_CHAIN_FLAG(desc.Flags as i32),
                    )
                };
                rtv = target()?;
            }
            presents += 1;
        }
        if args.fullscreen {
            // SAFETY: leave fullscreen before the swap chain is released.
            let _ = unsafe { swap.SetFullscreenState(false, None) };
        }
        Ok(())
    }
}
