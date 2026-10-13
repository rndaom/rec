//! Adapter selection and D3D11 device creation.

use windows::Win32::Foundation::{HMODULE, HWND, LUID, POINT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
    MonitorFromPoint, MonitorFromWindow,
};
use windows::Win32::UI::WindowsAndMessaging::IsWindow;
use windows::core::Interface;

use crate::util::{Context, from_wide};
use crate::{CaptureSource, VideoError};

/// The D3D11 device shared by capture, colour conversion and the encoder.
#[derive(Debug)]
pub(crate) struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub adapter_name: String,
    pub luid: LUID,
    /// PCI vendor id (0x1002 AMD, 0x10DE NVIDIA, 0x8086 Intel).
    #[allow(dead_code)] // TEMP: used once the AMF backend is wired in
    pub vendor_id: u32,
    /// The adapter is the one whose output shows the target monitor.
    pub owns_monitor: bool,
}

/// PCI vendor id of AMD adapters.
#[allow(dead_code)] // TEMP: used once the AMF backend is wired in
pub(crate) const VENDOR_AMD: u32 = 0x1002;

pub(crate) fn hwnd(raw: isize) -> HWND {
    HWND(raw as *mut core::ffi::c_void)
}

/// The monitor the capture source is on.
pub(crate) fn target_monitor(source: CaptureSource) -> HMONITOR {
    match source {
        // SAFETY: MonitorFromWindow accepts any HWND value; an invalid one
        // yields the nearest/primary monitor, and WGC rejects it later.
        CaptureSource::Window(h) => unsafe { MonitorFromWindow(hwnd(h), MONITOR_DEFAULTTONEAREST) },
        CaptureSource::Monitor(h) => HMONITOR(h as *mut core::ffi::c_void),
        // SAFETY: plain query; the origin is always on the primary monitor.
        CaptureSource::PrimaryMonitor => unsafe {
            MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY)
        },
    }
}

/// False once the captured window has been destroyed or the monitor
/// disconnected. `GraphicsCaptureItem.Closed` is not reliably raised for
/// window items (observed on build 26200), so the encode thread also polls
/// this once per second.
pub(crate) fn source_alive(source: CaptureSource) -> bool {
    match source {
        // SAFETY: IsWindow accepts any value and only reads window state.
        CaptureSource::Window(h) => unsafe { IsWindow(Some(hwnd(h))) }.as_bool(),
        CaptureSource::Monitor(h) => {
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            // SAFETY: `info` is a correctly sized out-struct; an invalid
            // HMONITOR just makes the call fail.
            unsafe { GetMonitorInfoW(HMONITOR(h as *mut core::ffi::c_void), &mut info) }.as_bool()
        }
        CaptureSource::PrimaryMonitor => true,
    }
}

/// A LUID as one comparable number.
pub(crate) fn luid_key(luid: LUID) -> i64 {
    (i64::from(luid.HighPart) << 32) | i64::from(luid.LowPart)
}

/// LUID of the adapter [`create_for_source`] would pick for `source` now.
pub(crate) fn adapter_luid(source: CaptureSource) -> Result<i64, VideoError> {
    let (adapter, _) = find_adapter(target_monitor(source))?;
    // SAFETY: GetDesc1 fills a plain struct.
    let desc = unsafe { adapter.GetDesc1() }.ctx("IDXGIAdapter1::GetDesc1")?;
    Ok(luid_key(desc.AdapterLuid))
}

/// Finds the adapter whose DXGI output is `monitor`, with that output. Falls
/// back to the first hardware adapter (e.g. for indirect/virtual displays)
/// and no output.
pub(crate) fn find_adapter(
    monitor: HMONITOR,
) -> Result<(IDXGIAdapter1, Option<IDXGIOutput>), VideoError> {
    // SAFETY: plain factory creation.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ctx("CreateDXGIFactory1")?;
    let mut first = None;
    for i in 0.. {
        // SAFETY: EnumAdapters1 returns DXGI_ERROR_NOT_FOUND past the end.
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
            break;
        };
        for j in 0.. {
            // SAFETY: as above, for outputs.
            let Ok(output) = (unsafe { adapter.EnumOutputs(j) }) else {
                break;
            };
            // SAFETY: GetDesc fills a plain struct.
            if unsafe { output.GetDesc() }.is_ok_and(|desc| desc.Monitor == monitor) {
                return Ok((adapter, Some(output)));
            }
        }
        if first.is_none() {
            first = Some(adapter);
        }
    }
    first
        .map(|a| (a, None))
        .ok_or_else(|| VideoError::Other("no DXGI adapter found".into()))
}

/// Creates the D3D11 device on the adapter that drives `source`'s monitor.
pub(crate) fn create_for_source(source: CaptureSource) -> Result<Gpu, VideoError> {
    let (adapter, output) = find_adapter(target_monitor(source))?;
    let owns_monitor = output.is_some();
    // SAFETY: GetDesc1 fills a plain struct.
    let desc = unsafe { adapter.GetDesc1() }.ctx("IDXGIAdapter1::GetDesc1")?;

    let mut device = None;
    let mut context = None;
    // SAFETY: valid adapter; out-pointers are valid for writes. With an
    // explicit adapter the driver type must be UNKNOWN.
    unsafe {
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .ctx("D3D11CreateDevice")?;
    let (Some(device), Some(context)) = (device, context) else {
        return Err(VideoError::Other(
            "D3D11CreateDevice returned no device".into(),
        ));
    };

    // The capture callback, the encode thread and the encoder MFT all use the
    // immediate context.
    let multithread: ID3D11Multithread = context.cast().ctx("ID3D11Multithread")?;
    // SAFETY: simple flag set on a valid interface.
    unsafe {
        let _ = multithread.SetMultithreadProtected(true);
    }

    Ok(Gpu {
        device,
        context,
        adapter_name: from_wide(&desc.Description),
        luid: desc.AdapterLuid,
        vendor_id: desc.VendorId,
        owns_monitor,
    })
}
