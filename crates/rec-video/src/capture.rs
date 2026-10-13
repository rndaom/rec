//! Windows.Graphics.Capture: item, free-threaded frame pool, session, and the
//! FrameArrived handler that copies the newest frame into the "latest"
//! texture.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use windows::Foundation::Metadata::ApiInformation;
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureAccess, GraphicsCaptureAccessKind,
    GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Security::Authorization::AppCapabilityAccess::AppCapabilityAccessStatus;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{HSTRING, Interface, Ref};

use crate::d3d::{Gpu, hwnd};
use crate::util::Context;
use crate::{BorderlessAccess, CaptureSource, SharedStats, VideoError};

const SESSION_CLASS: &str = "Windows.Graphics.Capture.GraphicsCaptureSession";
const PIXEL_FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;
const POOL_BUFFERS: i32 = 2;

/// The most recent captured frame, owned by us (copied out of the pool).
#[derive(Debug, Default)]
pub(crate) struct Latest {
    pub texture: Option<ID3D11Texture2D>,
    pub tex_width: u32,
    pub tex_height: u32,
    /// Valid region of `texture` (the content can be smaller than the
    /// surface right after a window resize).
    pub content_width: u32,
    pub content_height: u32,
    /// Increments with every copied frame; 0 = nothing captured yet.
    pub seq: u64,
}

#[derive(Debug)]
pub(crate) struct CaptureShared {
    pub latest: Mutex<Latest>,
    pub stats: Arc<SharedStats>,
}

pub(crate) struct Capture {
    _item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    frame_token: i64,
    closed_token: Option<(GraphicsCaptureItem, i64)>,
    pub source_width: u32,
    pub source_height: u32,
    pub border_required: Option<bool>,
    pub min_update_interval_hns: Option<i64>,
}

impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Capture")
            .field("source_width", &self.source_width)
            .field("source_height", &self.source_height)
            .finish_non_exhaustive()
    }
}

fn property_present(name: &str) -> bool {
    ApiInformation::IsPropertyPresent(&HSTRING::from(SESSION_CLASS), &HSTRING::from(name))
        .unwrap_or(false)
}

fn create_item(source: CaptureSource) -> Result<GraphicsCaptureItem, VideoError> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
        .ctx("IGraphicsCaptureItemInterop")?;
    match source {
        CaptureSource::Window(h) => {
            // SAFETY: the interop call validates the HWND and fails for bad ones.
            unsafe { interop.CreateForWindow(hwnd(h)) }.ctx("GraphicsCaptureItem::CreateForWindow")
        }
        CaptureSource::Monitor(_) | CaptureSource::PrimaryMonitor => {
            let monitor: HMONITOR = crate::d3d::target_monitor(source);
            // SAFETY: as above, for monitors.
            unsafe { interop.CreateForMonitor(monitor) }
                .ctx("GraphicsCaptureItem::CreateForMonitor")
        }
    }
}

impl Capture {
    /// Creates the capture item, pool and session. Frames start flowing on
    /// [`Capture::start`].
    pub(crate) fn new(
        source: CaptureSource,
        gpu: &Gpu,
        fps: u32,
        cursor: bool,
        shared: Arc<CaptureShared>,
    ) -> Result<Self, VideoError> {
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            return Err(VideoError::CaptureUnsupported);
        }
        let item = create_item(source)?;
        let size = item.Size().ctx("GraphicsCaptureItem::Size")?;
        if size.Width <= 0 || size.Height <= 0 {
            return Err(VideoError::Other(format!(
                "capture source has an empty size ({}x{}); is the window minimised?",
                size.Width, size.Height
            )));
        }

        let dxgi: IDXGIDevice = gpu.device.cast().ctx("IDXGIDevice")?;
        // SAFETY: valid DXGI device; returns a WinRT IDirect3DDevice wrapper.
        let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .ctx("CreateDirect3D11DeviceFromDXGIDevice")?;
        let rt_device: IDirect3DDevice = inspectable.cast().ctx("IDirect3DDevice")?;

        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &rt_device,
            PIXEL_FORMAT,
            POOL_BUFFERS,
            size,
        )
        .ctx("Direct3D11CaptureFramePool::CreateFreeThreaded")?;

        let handler = FrameHandler {
            device: gpu.device.clone(),
            context: gpu.context.clone(),
            rt_device: AgileDevice(rt_device),
            pool_size: Mutex::new(size),
            shared: shared.clone(),
        };
        let frame_token = pool
            .FrameArrived(&TypedEventHandler::new(
                move |sender: Ref<Direct3D11CaptureFramePool>, _| {
                    if let Some(pool) = sender.as_ref() {
                        if let Err(e) = handler.on_frame(pool) {
                            handler.shared.stats.fail(e);
                        }
                    }
                    Ok(())
                },
            ))
            .ctx("FrameArrived")?;

        let closed_stats = shared.stats.clone();
        let closed_token = item
            .Closed(&TypedEventHandler::new(move |_, _| {
                closed_stats.source_closed.store(true, Ordering::Release);
                Ok(())
            }))
            .ctx("GraphicsCaptureItem::Closed")?;

        let session = pool
            .CreateCaptureSession(&item)
            .ctx("CreateCaptureSession")?;

        if property_present("IsCursorCaptureEnabled") {
            session
                .SetIsCursorCaptureEnabled(cursor)
                .ctx("SetIsCursorCaptureEnabled")?;
        }

        let border_required = if property_present("IsBorderRequired") {
            Some(disable_border(&session))
        } else {
            None
        };

        let min_update_interval_hns = if property_present("MinUpdateInterval") {
            let interval = min_update_interval(fps);
            session
                .SetMinUpdateInterval(TimeSpan { Duration: interval })
                .ctx("SetMinUpdateInterval")?;
            Some(interval)
        } else {
            None
        };

        Ok(Self {
            closed_token: Some((item.clone(), closed_token)),
            _item: item,
            pool,
            session,
            frame_token,
            source_width: size.Width as u32,
            source_height: size.Height as u32,
            border_required,
            min_update_interval_hns,
        })
    }

    pub(crate) fn start(&self) -> Result<(), VideoError> {
        self.session
            .StartCapture()
            .ctx("GraphicsCaptureSession::StartCapture")
    }
}

/// `MinUpdateInterval` for an encode rate of `fps`.
///
/// Half the tick period rather than a full one: WGC delivers the first game
/// frame presented *after* the interval has elapsed, so with interval = 1/fps
/// a 144 Hz game would arrive every 3 game frames (20.8 ms, 48 fps) and a
/// 60 fps encode would repeat every fourth frame. Half the period still caps
/// the copies at 2 × fps while every tick sees a fresh frame whenever the
/// game renders at least `fps`.
pub(crate) fn min_update_interval(fps: u32) -> i64 {
    rec_core::time::HNS_PER_SEC / i64::from(fps.max(1)) / 2
}

/// Turns the yellow border off. Returns the resulting `IsBorderRequired`.
fn disable_border(session: &GraphicsCaptureSession) -> bool {
    if session.SetIsBorderRequired(false).is_err() {
        // Packaged apps need the graphicsCaptureWithoutBorder capability or
        // user consent; ask once and retry.
        let _ = request_borderless_access();
        let _ = session.SetIsBorderRequired(false);
    }
    session.IsBorderRequired().unwrap_or(true)
}

pub(crate) fn request_borderless_access() -> Result<BorderlessAccess, VideoError> {
    let present = ApiInformation::IsTypePresent(&HSTRING::from(
        "Windows.Graphics.Capture.GraphicsCaptureAccess",
    ))
    .unwrap_or(false);
    if !present {
        return Ok(BorderlessAccess::Unsupported);
    }
    let status = GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless)
        .ctx("GraphicsCaptureAccess::RequestAccessAsync")?
        .join()
        .ctx("RequestAccessAsync result")?;
    Ok(match status {
        AppCapabilityAccessStatus::Allowed => BorderlessAccess::Allowed,
        AppCapabilityAccessStatus::DeniedByUser => BorderlessAccess::DeniedByUser,
        AppCapabilityAccessStatus::DeniedBySystem => BorderlessAccess::DeniedBySystem,
        AppCapabilityAccessStatus::NotDeclaredByApp => BorderlessAccess::NotDeclaredByApp,
        AppCapabilityAccessStatus::UserPromptRequired => BorderlessAccess::UserPromptRequired,
        _ => BorderlessAccess::Unknown,
    })
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.pool.RemoveFrameArrived(self.frame_token);
        if let Some((item, token)) = self.closed_token.take() {
            let _ = item.RemoveClosed(token);
        }
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// The WinRT device wrapper, made `Send` for the FrameArrived closure.
struct AgileDevice(IDirect3DDevice);
// SAFETY: the object returned by CreateDirect3D11DeviceFromDXGIDevice is
// free-threaded (agile) and wraps a multithread-protected D3D11 device;
// windows-rs only lacks the marker because the interface is not declared agile.
unsafe impl Send for AgileDevice {}
// SAFETY: as above; it is only passed by reference to `Recreate`.
unsafe impl Sync for AgileDevice {}

/// State owned by the FrameArrived closure (runs on a thread-pool thread).
struct FrameHandler {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    rt_device: AgileDevice,
    pool_size: Mutex<SizeInt32>,
    shared: Arc<CaptureShared>,
}

impl FrameHandler {
    fn on_frame(&self, pool: &Direct3D11CaptureFramePool) -> Result<(), VideoError> {
        // A spurious event with no frame is not an error.
        let Ok(frame) = pool.TryGetNextFrame() else {
            return Ok(());
        };
        let content = frame.ContentSize().ctx("ContentSize")?;
        let surface = frame.Surface().ctx("Direct3D11CaptureFrame::Surface")?;
        let access: IDirect3DDxgiInterfaceAccess =
            surface.cast().ctx("IDirect3DDxgiInterfaceAccess")?;
        // SAFETY: the surface wraps an ID3D11Texture2D on our device.
        let src: ID3D11Texture2D = unsafe { access.GetInterface() }.ctx("GetInterface")?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: GetDesc fills a plain struct.
        unsafe { src.GetDesc(&mut desc) };

        {
            let mut latest = self.shared.latest.lock().map_err(|_| poisoned())?;
            if latest.texture.is_none()
                || latest.tex_width != desc.Width
                || latest.tex_height != desc.Height
            {
                latest.texture = Some(self.create_latest(&desc)?);
                latest.tex_width = desc.Width;
                latest.tex_height = desc.Height;
            }
            if let Some(dst) = &latest.texture {
                // SAFETY: same size and format, both on our device; the
                // immediate context is multithread-protected.
                unsafe { self.context.CopyResource(dst, &src) };
            }
            latest.content_width = (content.Width.max(1) as u32).min(desc.Width);
            latest.content_height = (content.Height.max(1) as u32).min(desc.Height);
            latest.seq += 1;
        }
        // Hand the buffer back to the pool as early as possible.
        drop(src);
        let _ = frame.Close();
        self.shared.stats.captured.fetch_add(1, Ordering::Relaxed);

        // Follow window resizes so the pool buffers match the content.
        if content.Width > 0 && content.Height > 0 {
            let mut size = self.pool_size.lock().map_err(|_| poisoned())?;
            if content != *size {
                pool.Recreate(&self.rt_device.0, PIXEL_FORMAT, POOL_BUFFERS, content)
                    .ctx("Direct3D11CaptureFramePool::Recreate")?;
                *size = content;
            }
        }
        Ok(())
    }

    fn create_latest(&self, src: &D3D11_TEXTURE2D_DESC) -> Result<ID3D11Texture2D, VideoError> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: src.Width,
            Height: src.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: src.Format,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        // SAFETY: valid descriptor; out-pointer valid for writes.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .ctx("CreateTexture2D(latest)")?;
        texture.ok_or_else(|| VideoError::Other("CreateTexture2D returned null".into()))
    }
}

fn poisoned() -> VideoError {
    VideoError::Other("capture state lock poisoned".into())
}
