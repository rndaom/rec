//! Desktop Duplication (DDA): the capture source where WGC cannot turn its
//! yellow border off (Windows 10; see [`crate::api`]). It duplicates the
//! display the source is on and copies the source window's part of each
//! desktop image into the same "latest" texture the WGC path fills, in the
//! same format, so conversion and encoding do not know the difference.
//!
//! DDA has no callback: a thread of its own polls `AcquireNextFrame` every
//! [`crate::capture::min_update_interval`] (half the encode tick, as WGC's
//! `MinUpdateInterval`), without waiting: while `AcquireNextFrame` waits it
//! holds the D3D11 device's multithread lock, which stalled the encode
//! thread (84 of 150 ticks dropped on a still desktop with a 50 ms wait).
//! DDA accumulates the updates between two polls, so each poll gets the
//! newest picture. Like WGC, a frame only arrives when the desktop changed,
//! and the encode thread repeats the last picture in between.
//!
//! What DDA does differently, and how it is handled:
//! * **Access lost.** A desktop switch (UAC, Ctrl+Alt+Del, the lock screen),
//!   a mode change or a fullscreen transition invalidates the duplication.
//!   It is recreated, at once and then with a growing delay; meanwhile the
//!   last picture repeats. Only errors that persist for [`GIVE_UP`] (or a
//!   lost device) stop the pipeline, which recd then restarts
//!   (`recd/src/recover.rs`). The secure desktop (UAC prompt, lock screen)
//!   can stay up for any length of time and is waited out.
//! * **The window moves.** Its rectangle (DWM's extended frame bounds: what
//!   WGC captures, without the drop shadow) is read for every frame, so
//!   moves and resizes follow at once. A resize shows in the stats' source
//!   size, and recd restarts the pipeline at the new size like with WGC. A
//!   move to another display on the same GPU re-duplicates that display; to
//!   a display on another GPU, the last picture repeats until recd restarts
//!   the pipeline on that GPU.
//! * **Covering windows.** DDA sees the desktop, so whatever is on top of
//!   the game window (a notification, another window) is recorded. Windows
//!   marked `WDA_EXCLUDEFROMCAPTURE`, like rec's own overlay, are not.
//! * **The cursor** is not part of the desktop image (DDA reports its shape
//!   and position separately) and is not drawn. Games hide the system cursor
//!   or draw their own, which is captured.
//! * **Rotated displays** deliver the desktop image unrotated. Rather than
//!   rotate it, `CaptureApi::Auto` uses WGC (with its border) there, and a
//!   forced `Dda` fails with a clear error.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rec_core::time::qpc_now;
use windows::Win32::Foundation::{E_ACCESSDENIED, RECT};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_MODE_ROTATION,
    DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_INVALID_CALL, DXGI_ERROR_MODE_CHANGE_IN_PROGRESS,
    DXGI_ERROR_NOT_CURRENTLY_AVAILABLE, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_SESSION_DISCONNECTED,
    DXGI_ERROR_UNSUPPORTED, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTPUT_DESC,
    IDXGIDevice, IDXGIOutput, IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsIconic};
use windows::core::Interface;

use crate::capture::{CaptureFormat, CaptureShared, create_latest, poisoned};
use crate::d3d::{Gpu, hwnd, target_monitor};
use crate::geometry::{Rect, ScreenRect, crop};
use crate::util::Context;
use crate::{CaptureSource, VideoError};

/// Duplication that keeps failing this long (other than for the secure
/// desktop or a display on another GPU) stops the pipeline, for recd to
/// restart.
const GIVE_UP: Duration = Duration::from_secs(30);

/// Why a duplication could not be created or stopped delivering.
#[derive(Debug)]
struct Lost {
    error: VideoError,
    /// Wait indefinitely: the secure desktop is up, or the source is on a
    /// display of another GPU (recd moves the pipeline there).
    patient: bool,
}

impl From<VideoError> for Lost {
    fn from(error: VideoError) -> Self {
        Self {
            error,
            patient: false,
        }
    }
}

/// One duplicated display.
struct Duplication {
    dup: IDXGIOutputDuplication,
    /// The `HMONITOR`, as a number (handles are not `Send`).
    monitor: isize,
    /// The display's place on the virtual screen.
    desktop: ScreenRect,
}

/// The pipeline's DDA capture source. Frames start flowing on
/// [`DdaCapture::start`].
pub(crate) struct DdaCapture {
    pub source_width: u32,
    pub source_height: u32,
    pub min_update_interval_hns: i64,
    worker: Option<Worker>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for DdaCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DdaCapture")
            .field("source_width", &self.source_width)
            .field("source_height", &self.source_height)
            .finish_non_exhaustive()
    }
}

/// Whether DDA can capture `monitor` with `gpu`: DXGI lists the display as
/// one of the GPU's outputs, and it is not rotated.
pub(crate) fn usable(gpu: &Gpu, monitor: HMONITOR) -> bool {
    output_of(&gpu.device, monitor)
        .is_ok_and(|found| found.is_some_and(|(_, desc)| !rotated(desc.Rotation)))
}

fn rotated(rotation: DXGI_MODE_ROTATION) -> bool {
    rotation != DXGI_MODE_ROTATION_IDENTITY && rotation != DXGI_MODE_ROTATION_UNSPECIFIED
}

/// `monitor`'s output on the adapter `device` lives on, if it has one.
fn output_of(
    device: &ID3D11Device,
    monitor: HMONITOR,
) -> Result<Option<(IDXGIOutput, DXGI_OUTPUT_DESC)>, VideoError> {
    let dxgi: IDXGIDevice = device.cast().ctx("IDXGIDevice")?;
    // SAFETY: plain getter on a valid device.
    let adapter = unsafe { dxgi.GetAdapter() }.ctx("IDXGIDevice::GetAdapter")?;
    for i in 0.. {
        // SAFETY: EnumOutputs returns DXGI_ERROR_NOT_FOUND past the end.
        let Ok(output) = (unsafe { adapter.EnumOutputs(i) }) else {
            break;
        };
        // SAFETY: GetDesc fills a plain struct.
        if let Ok(desc) = unsafe { output.GetDesc() } {
            if desc.Monitor == monitor {
                return Ok(Some((output, desc)));
            }
        }
    }
    Ok(None)
}

fn dxgi_format(format: CaptureFormat) -> DXGI_FORMAT {
    match format {
        CaptureFormat::Bgra8 => DXGI_FORMAT_B8G8R8A8_UNORM,
        CaptureFormat::Fp16 => DXGI_FORMAT_R16G16B16A16_FLOAT,
    }
}

/// Duplicates `monitor`'s display in `format`.
fn duplicate(
    device: &ID3D11Device,
    monitor: HMONITOR,
    format: CaptureFormat,
) -> Result<Duplication, Lost> {
    let Some((output, desc)) = output_of(device, monitor)? else {
        return Err(Lost {
            error: VideoError::Other("the display is not driven by the capture GPU".into()),
            patient: true,
        });
    };
    if rotated(desc.Rotation) {
        return Err(VideoError::Other(
            "a rotated display cannot be captured; set video.capture = \"wgc\"".into(),
        )
        .into());
    }
    // DuplicateOutput1 (Windows 10 1703+) delivers the format asked for:
    // FP16 scRGB keeps HDR, and an 8-bit request of an HDR desktop gets the
    // SDR picture Windows would show. The older call only knows 8-bit.
    let dup = match output.cast::<IDXGIOutput5>() {
        // SAFETY: valid device and output; the format list outlives the call.
        Ok(output5) => unsafe { output5.DuplicateOutput1(device, 0, &[dxgi_format(format)]) }
            .ctx("IDXGIOutput5::DuplicateOutput1")
            .or_else(|e| match format {
                CaptureFormat::Bgra8 => duplicate_v1(&output, device).map_err(|_| e),
                CaptureFormat::Fp16 => Err(e),
            }),
        Err(_) if format == CaptureFormat::Bgra8 => duplicate_v1(&output, device),
        Err(_) => Err(VideoError::Other(
            "HDR capture with Desktop Duplication needs Windows 10 1703 or later".into(),
        )),
    };
    let dup = dup.map_err(|error| Lost {
        patient: is_code(&error, E_ACCESSDENIED.0),
        error,
    })?;
    let r = desc.DesktopCoordinates;
    Ok(Duplication {
        dup,
        monitor: monitor.0 as isize,
        desktop: ScreenRect {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        },
    })
}

fn duplicate_v1(
    output: &IDXGIOutput,
    device: &ID3D11Device,
) -> Result<IDXGIOutputDuplication, VideoError> {
    let output1: IDXGIOutput1 = output.cast().ctx("IDXGIOutput1")?;
    // SAFETY: valid device and output.
    unsafe { output1.DuplicateOutput(device) }.ctx("IDXGIOutput1::DuplicateOutput")
}

fn is_code(error: &VideoError, code: i32) -> bool {
    matches!(error, VideoError::Windows { code: c, .. } if *c == code as u32)
}

/// Errors after which a new duplication may work: the desktop switched
/// (UAC, lock screen), the display mode is changing, or a fullscreen game
/// took over or let go of the output. Anything else (a lost device above
/// all) stops the pipeline.
fn transient(error: &VideoError) -> bool {
    [
        DXGI_ERROR_ACCESS_LOST,
        DXGI_ERROR_INVALID_CALL,
        DXGI_ERROR_MODE_CHANGE_IN_PROGRESS,
        DXGI_ERROR_NOT_CURRENTLY_AVAILABLE,
        DXGI_ERROR_NOT_FOUND,
        DXGI_ERROR_SESSION_DISCONNECTED,
        DXGI_ERROR_UNSUPPORTED,
        E_ACCESSDENIED,
    ]
    .iter()
    .any(|code| is_code(error, code.0))
}

/// Wait before the `attempt`th try (0-based) to duplicate again: the first
/// at once, as after a fullscreen transition it usually works straight
/// away, then backing off to once a second.
fn retry_delay(attempt: u32) -> Duration {
    const DELAYS_MS: [u64; 5] = [0, 50, 100, 250, 500];
    Duration::from_millis(DELAYS_MS.get(attempt as usize).copied().unwrap_or(1000))
}

/// The window's visible rectangle on the virtual screen: DWM's extended
/// frame bounds (no drop shadow; physical pixels whatever the DPI
/// awareness), or the window rectangle where DWM has none.
fn window_bounds(h: isize) -> Option<ScreenRect> {
    let mut r = RECT::default();
    // SAFETY: `r` is a RECT, the size passed; a stale HWND makes it fail.
    let dwm = unsafe {
        DwmGetWindowAttribute(
            hwnd(h),
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut r as *mut RECT).cast(),
            size_of::<RECT>() as u32,
        )
    };
    // SAFETY: as above.
    if dwm.is_err() && unsafe { GetWindowRect(hwnd(h), &mut r) }.is_err() {
        return None;
    }
    Some(ScreenRect {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    })
}

/// The part of the duplicated display to record, relative to it.
fn region(source: CaptureSource, desktop: ScreenRect) -> Option<Rect> {
    match source {
        CaptureSource::Window(h) => {
            // SAFETY: IsIconic accepts any value and only reads window state.
            if unsafe { IsIconic(hwnd(h)) }.as_bool() {
                return None;
            }
            crop(window_bounds(h)?, desktop)
        }
        CaptureSource::Monitor(_) | CaptureSource::PrimaryMonitor => crop(desktop, desktop),
    }
}

impl DdaCapture {
    /// Duplicates the source's display. Frames start flowing on
    /// [`DdaCapture::start`].
    pub(crate) fn new(
        source: CaptureSource,
        gpu: &Gpu,
        fps: u32,
        format: CaptureFormat,
        shared: Arc<CaptureShared>,
    ) -> Result<Self, VideoError> {
        let monitor = target_monitor(source);
        let dup = duplicate(&gpu.device, monitor, format).map_err(|lost| lost.error)?;
        let Some(region) = region(source, dup.desktop) else {
            return Err(VideoError::Other(
                "the window is not on its display; is it minimised?".into(),
            ));
        };
        let interval = crate::capture::min_update_interval(fps);
        let stop = Arc::new(AtomicBool::new(false));
        Ok(Self {
            source_width: region.width(),
            source_height: region.height(),
            min_update_interval_hns: interval,
            worker: Some(Worker {
                source,
                device: gpu.device.clone(),
                context: gpu.context.clone(),
                format,
                interval: Duration::from_nanos(interval.max(0) as u64 * 100),
                shared,
                stop: stop.clone(),
                dup: Some(dup),
                reported_format: false,
            }),
            stop,
            thread: None,
        })
    }

    pub(crate) fn start(&mut self) -> Result<(), VideoError> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        let thread = std::thread::Builder::new()
            .name("rec-video-dda".into())
            .spawn(move || worker.run())
            .map_err(|e| VideoError::Other(format!("cannot spawn capture thread: {e}")))?;
        self.thread = Some(thread);
        Ok(())
    }
}

impl Drop for DdaCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // Returns within one poll interval.
            let _ = thread.join();
        }
    }
}

/// State of the capture thread.
struct Worker {
    source: CaptureSource,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    format: CaptureFormat,
    /// Least time between two acquired frames.
    interval: Duration,
    shared: Arc<CaptureShared>,
    stop: Arc<AtomicBool>,
    dup: Option<Duplication>,
    reported_format: bool,
}

impl Worker {
    fn run(mut self) {
        // Failed tries since a frame was last copied: a duplication that
        // is created and lost again straight away (exclusive fullscreen)
        // backs off like one that cannot be created.
        let mut attempt = 0;
        let mut retry_at = Instant::now();
        // Since when there has been no working duplication.
        let mut lost_since: Option<Instant> = None;
        while !self.stop.load(Ordering::Acquire) {
            // Follow the window to another display.
            if let (Some(dup), CaptureSource::Window(_)) = (&self.dup, self.source) {
                if target_monitor(self.source).0 as isize != dup.monitor {
                    self.dup = None;
                    lost_since.get_or_insert_with(Instant::now);
                    (attempt, retry_at) = (0, Instant::now());
                }
            }
            if self.dup.is_none() {
                let now = Instant::now();
                if now < retry_at {
                    std::thread::sleep((retry_at - now).min(Duration::from_millis(50)));
                    continue;
                }
                match duplicate(&self.device, target_monitor(self.source), self.format) {
                    Ok(dup) => {
                        self.dup = Some(dup);
                        lost_since = None;
                    }
                    Err(lost) => {
                        let since = *lost_since.get_or_insert(now);
                        let hopeless =
                            !lost.patient && (!transient(&lost.error) || now - since >= GIVE_UP);
                        if hopeless {
                            self.shared
                                .stats
                                .fail(format!("Desktop Duplication unavailable: {}", lost.error));
                            return;
                        }
                        retry_at = now + retry_delay(attempt);
                        attempt += 1;
                    }
                }
                continue;
            }
            let started = Instant::now();
            match self.next_frame() {
                Ok(copied) => {
                    if copied {
                        attempt = 0;
                    }
                    let next = started + self.interval;
                    let now = Instant::now();
                    if next > now {
                        std::thread::sleep(next - now);
                    }
                }
                Err(e) if transient(&e) => {
                    // Recreate: at once if frames were flowing.
                    self.dup = None;
                    let now = Instant::now();
                    lost_since = Some(now);
                    retry_at = now + retry_delay(attempt);
                    attempt += 1;
                }
                Err(e) => {
                    self.shared.stats.fail(e);
                    return;
                }
            }
        }
    }

    /// Waits for the next desktop image and copies the source's part of it.
    /// Returns whether a new picture was copied.
    fn next_frame(&mut self) -> Result<bool, VideoError> {
        let Some(Duplication { dup, desktop, .. }) = &self.dup else {
            return Ok(false);
        };
        let (dup, desktop) = (dup.clone(), *desktop);
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: out-pointers valid for writes; a frame acquired here is
        // released below on every path.
        let acquired = unsafe { dup.AcquireNextFrame(0, &mut info, &mut resource) };

        match acquired {
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
            other => other.ctx("IDXGIOutputDuplication::AcquireNextFrame")?,
        }
        // Only the pointer moved when no present time is given. (The first
        // frame of a new duplication has one too, so a still desktop shows
        // at once.)
        let copied = if info.LastPresentTime == 0 {
            Ok(false)
        } else {
            self.copy(resource, desktop)
        };
        // SAFETY: a frame is held (acquired above).
        let released = unsafe { dup.ReleaseFrame() }.ctx("IDXGIOutputDuplication::ReleaseFrame");
        let copied = copied?;
        released?;
        Ok(copied)
    }

    fn copy(
        &mut self,
        resource: Option<IDXGIResource>,
        desktop: ScreenRect,
    ) -> Result<bool, VideoError> {
        let Some(region) = region(self.source, desktop) else {
            // Minimised or off this display: keep repeating the last picture.
            return Ok(false);
        };
        let Some(resource) = resource else {
            return Ok(false);
        };
        let src: ID3D11Texture2D = resource.cast().ctx("desktop image ID3D11Texture2D")?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: GetDesc fills a plain struct.
        unsafe { src.GetDesc(&mut desc) };
        if desc.Format != dxgi_format(self.format) {
            // The converter is built for one format; recd restarts the
            // pipeline when the display's HDR mode changes.
            if !self.reported_format {
                self.reported_format = true;
                self.shared.stats.set_error(format!(
                    "Desktop Duplication delivered format {} instead of {}; frames skipped",
                    desc.Format.0,
                    dxgi_format(self.format).0
                ));
            }
            return Ok(false);
        }
        let region = Rect {
            right: region.right.min(desc.Width),
            bottom: region.bottom.min(desc.Height),
            ..region
        };
        if region.right <= region.left || region.bottom <= region.top {
            return Ok(false);
        }
        {
            let mut latest = self.shared.latest.lock().map_err(|_| poisoned())?;
            // As big as the display, so moving or resizing the window never
            // reallocates it; the picture is its top-left corner.
            if latest.texture.is_none()
                || latest.tex_width != desc.Width
                || latest.tex_height != desc.Height
            {
                latest.texture = Some(create_latest(
                    &self.device,
                    desc.Width,
                    desc.Height,
                    desc.Format,
                )?);
                latest.tex_width = desc.Width;
                latest.tex_height = desc.Height;
            }
            if let Some(dst) = &latest.texture {
                let area = D3D11_BOX {
                    left: region.left,
                    top: region.top,
                    front: 0,
                    right: region.right,
                    bottom: region.bottom,
                    back: 1,
                };
                // SAFETY: same format, both on our device, the box lies
                // inside the source and fits the destination at (0, 0); the
                // immediate context is multithread-protected.
                unsafe {
                    self.context
                        .CopySubresourceRegion(dst, 0, 0, 0, 0, &src, 0, Some(&area))
                };
            }
            latest.content_width = region.width();
            latest.content_height = region.height();
            latest.seq += 1;
        }
        let stats = &self.shared.stats;
        stats.captured.fetch_add(1, Ordering::Relaxed);
        stats.last_frame.store(qpc_now(), Ordering::Relaxed);
        let size = (u64::from(region.width()) << 32) | u64::from(region.height());
        stats.source_size.store(size, Ordering::Relaxed);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(code: windows::core::HRESULT) -> VideoError {
        VideoError::Windows {
            context: "test",
            code: code.0 as u32,
            message: String::new(),
        }
    }

    #[test]
    fn desktop_switches_and_mode_changes_are_retried() {
        for code in [
            DXGI_ERROR_ACCESS_LOST,
            E_ACCESSDENIED,
            DXGI_ERROR_MODE_CHANGE_IN_PROGRESS,
            DXGI_ERROR_SESSION_DISCONNECTED,
        ] {
            assert!(transient(&win(code)), "{code:?}");
        }
    }

    #[test]
    fn a_lost_device_is_not_retried() {
        use windows::Win32::Graphics::Dxgi::{DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED};
        assert!(!transient(&win(DXGI_ERROR_DEVICE_REMOVED)));
        assert!(!transient(&win(DXGI_ERROR_DEVICE_HUNG)));
        assert!(!transient(&VideoError::Other("x".into())));
    }

    #[test]
    fn retries_start_at_once_and_back_off_to_a_second() {
        assert_eq!(retry_delay(0), Duration::ZERO);
        let delays: Vec<_> = (0..10).map(retry_delay).collect();
        assert!(delays.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(retry_delay(100), Duration::from_secs(1));
    }

    #[test]
    fn any_turn_counts_as_rotated() {
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_MODE_ROTATION_ROTATE90, DXGI_MODE_ROTATION_ROTATE180,
        };
        assert!(!rotated(DXGI_MODE_ROTATION_IDENTITY));
        assert!(!rotated(DXGI_MODE_ROTATION_UNSPECIFIED));
        assert!(rotated(DXGI_MODE_ROTATION_ROTATE90));
        assert!(rotated(DXGI_MODE_ROTATION_ROTATE180));
    }

    #[test]
    fn monitor_sources_record_the_whole_display() {
        let desktop = ScreenRect {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1080,
        };
        let r = region(CaptureSource::PrimaryMonitor, desktop).unwrap();
        assert_eq!((r.left, r.top, r.width(), r.height()), (0, 0, 1920, 1080));
    }
}
