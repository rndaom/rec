//! A synthetic game that keeps the GPU at 100%: a D3D11 window that draws a
//! heavy full-screen pixel shader several times per frame and presents
//! without waiting for vsync, like an uncapped game. For testing how the
//! capture pipeline copes when the game saturates the GPU (see the
//! `--gpu-priority` option of the `record` example).
//!
//! ```text
//! cargo run --release -p rec-video --example gpu_stress -- [--seconds N] \
//!     [--iterations N] [--draws N] [--idle-after S] [--resume-after S]
//! ```
//!
//! Each draw runs `--iterations` loop iterations per pixel (default 400);
//! `--draws` (default 4) draws make one frame. `--idle-after` stops the
//! heavy draws (plain clears at vsync instead) and `--resume-after` starts
//! them again, to see recovery. Once a second it prints its frame rate.
//! The window title is "rec gpu stress". Press Esc to quit.

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
    use std::time::Instant;

    use windows::Win32::Foundation::{HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Direct3D::Fxc::{D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile};
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_HARDWARE, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_CONSTANT_BUFFER, D3D11_BUFFER_DESC, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        D3D11_SDK_VERSION, D3D11_USAGE_DEFAULT, D3D11_VIEWPORT, D3D11CreateDeviceAndSwapChain,
        ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView,
        ID3D11Texture2D, ID3D11VertexShader,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_DESC, DXGI_SAMPLE_DESC,
    };
    use windows::Win32::Graphics::Dxgi::{
        DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
        IDXGISwapChain,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
    use windows::Win32::UI::WindowsAndMessaging::{
        CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DispatchMessageW,
        GetSystemMetrics, MSG, PM_REMOVE, PeekMessageW, PostQuitMessage, RegisterClassW,
        SM_CXSCREEN, SM_CYSCREEN, SW_SHOW, SetForegroundWindow, ShowWindow, TranslateMessage,
        WM_DESTROY, WM_KEYDOWN, WM_QUIT, WNDCLASSW, WS_EX_APPWINDOW, WS_OVERLAPPEDWINDOW,
        WS_VISIBLE,
    };
    use windows::core::{PCSTR, w};

    const SHADER: &str = r"
cbuffer Frame : register(b0) { float time; uint iterations; float2 pad; };
float4 vs(uint id : SV_VertexID) : SV_Position {
    float2 uv = float2((id << 1) & 2, id & 2);
    return float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
}
float4 ps(float4 pos : SV_Position) : SV_Target {
    float2 z = pos.xy * 0.001;
    float a = time;
    [loop] for (uint i = 0; i < iterations; i++) {
        z = float2(z.x * z.x - z.y * z.y, 2 * z.x * z.y) * 0.5 + float2(sin(a), cos(a + i * 0.01));
        a += z.x * 0.001;
    }
    return float4(frac(z.x), frac(z.y), frac(a), 1);
}
";

    struct Args {
        seconds: f64,
        iterations: u32,
        draws: u32,
        idle_after: Option<f64>,
        resume_after: Option<f64>,
    }

    fn parse_args() -> Result<Args, String> {
        let mut a = Args {
            seconds: 30.0,
            iterations: 400,
            draws: 4,
            idle_after: None,
            resume_after: None,
        };
        let mut it = std::env::args().skip(1);
        let num = |v: Option<String>, name: &str| -> Result<f64, String> {
            v.and_then(|v| v.parse().ok())
                .ok_or(format!("{name} needs a number"))
        };
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--seconds" => a.seconds = num(it.next(), "--seconds")?,
                "--iterations" => a.iterations = num(it.next(), "--iterations")? as u32,
                "--draws" => a.draws = num(it.next(), "--draws")? as u32,
                "--idle-after" => a.idle_after = Some(num(it.next(), "--idle-after")?),
                "--resume-after" => a.resume_after = Some(num(it.next(), "--resume-after")?),
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

    fn compile(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob, String> {
        let mut code = None;
        let mut errors = None;
        // SAFETY: the source, entry and target strings outlive the call;
        // the out-pointers are valid locals.
        let result = unsafe {
            D3DCompile(
                SHADER.as_ptr().cast(),
                SHADER.len(),
                PCSTR(c"stress.hlsl".as_ptr().cast()),
                None,
                None,
                entry,
                target,
                D3DCOMPILE_OPTIMIZATION_LEVEL3,
                0,
                &mut code,
                Some(&mut errors),
            )
        };
        if let Err(e) = result {
            let detail = errors.map(|b| String::from_utf8_lossy(bytes(&b)).into_owned());
            return Err(format!("D3DCompile: {e} {}", detail.unwrap_or_default()));
        }
        code.ok_or_else(|| "D3DCompile returned no code".into())
    }

    fn bytes(blob: &ID3DBlob) -> &[u8] {
        // SAFETY: the blob owns that many bytes for as long as it lives.
        unsafe {
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
        }
    }

    pub fn main() -> Result<(), String> {
        let args = parse_args()?;
        // SAFETY: standard window class registration and creation with
        // static strings.
        let (hwnd, width, height) = unsafe {
            let instance: HINSTANCE = GetModuleHandleW(None).map_err(|e| e.to_string())?.into();
            let class = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                hInstance: instance,
                lpszClassName: w!("rec_gpu_stress"),
                ..Default::default()
            };
            RegisterClassW(&class);
            let (w, h) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
            let hwnd = CreateWindowExW(
                WS_EX_APPWINDOW,
                w!("rec_gpu_stress"),
                w!("rec gpu stress"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                0,
                0,
                w * 3 / 4,
                h * 3 / 4,
                None,
                None,
                Some(instance),
                None,
            )
            .map_err(|e| e.to_string())?;
            (hwnd, (w * 3 / 4) as f32, (h * 3 / 4) as f32)
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
            BufferCount: 2,
            OutputWindow: hwnd,
            Windowed: true.into(),
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            Flags: 0,
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

        let vs_code = compile(
            PCSTR(c"vs".as_ptr().cast()),
            PCSTR(c"vs_5_0".as_ptr().cast()),
        )?;
        let ps_code = compile(
            PCSTR(c"ps".as_ptr().cast()),
            PCSTR(c"ps_5_0".as_ptr().cast()),
        )?;
        let mut vs: Option<ID3D11VertexShader> = None;
        let mut ps: Option<ID3D11PixelShader> = None;
        let mut constants: Option<ID3D11Buffer> = None;
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        // SAFETY: valid bytecode and descriptors; out-pointers are locals.
        unsafe {
            device
                .CreateVertexShader(bytes(&vs_code), None, Some(&mut vs))
                .map_err(|e| e.to_string())?;
            device
                .CreatePixelShader(bytes(&ps_code), None, Some(&mut ps))
                .map_err(|e| e.to_string())?;
            let cb = D3D11_BUFFER_DESC {
                ByteWidth: 16,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            };
            device
                .CreateBuffer(&cb, None, Some(&mut constants))
                .map_err(|e| e.to_string())?;
            let buffer: ID3D11Texture2D = swap.GetBuffer(0).map_err(|e| e.to_string())?;
            device
                .CreateRenderTargetView(&buffer, None, Some(&mut rtv))
                .map_err(|e| e.to_string())?;
        }
        let (vs, ps, constants, rtv) = (vs.unwrap(), ps.unwrap(), constants.unwrap(), rtv.unwrap());

        let start = Instant::now();
        let mut last_print = 0u64;
        let mut frames = 0u64;
        let mut frames_at_print = 0u64;
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
                return Ok(());
            }
            let after = |s: Option<f64>| s.is_some_and(|s| t >= s);
            let idle = after(args.idle_after) && !after(args.resume_after);
            if t as u64 > last_print {
                last_print = t as u64;
                println!(
                    "t={last_print:>3}s {:>4} fps{}",
                    frames - frames_at_print,
                    if idle { " (idle)" } else { "" }
                );
                frames_at_print = frames;
            }
            let data: [u32; 4] = [(t as f32).to_bits(), args.iterations, 0, 0];
            // SAFETY: our own device objects, used on this thread only; the
            // constant data outlives the update.
            let hr = unsafe {
                context.ClearRenderTargetView(&rtv, &[0.1, 0.1, 0.1, 1.0]);
                if !idle {
                    context.UpdateSubresource(&constants, 0, None, data.as_ptr().cast(), 0, 0);
                    context.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
                    context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                        Width: width,
                        Height: height,
                        MaxDepth: 1.0,
                        ..Default::default()
                    }]));
                    context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                    context.VSSetShader(&vs, None);
                    context.PSSetShader(&ps, None);
                    context.PSSetConstantBuffers(0, Some(&[Some(constants.clone())]));
                    for _ in 0..args.draws {
                        context.Draw(3, 0);
                    }
                }
                swap.Present(u32::from(idle), Default::default())
            };
            if hr.is_err() {
                return Err(format!("Present: {hr:?}"));
            }
            frames += 1;
        }
    }
}
