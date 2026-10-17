//! Frames out of a saved clip with the Media Foundation source reader. It
//! decodes on the GPU through a D3D11 device when it can (rec's clips are
//! HEVC or AV1, which Windows may only decode in hardware), and its video
//! processor converts to 8-bit BGRA, optionally scaled. No FFmpeg (D4).
//!
//! Clips have an IDR frame every second, so a seek decodes at most a second
//! of video before the wanted frame. Requests in time order reuse the
//! decoder's position instead of seeking.

use std::path::Path;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11CreateDevice, ID3D11Device, ID3D11Multithread,
};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFAttributes, IMFDXGIDeviceManager, IMFMediaType, IMFSample, IMFSourceReader,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE,
    MF_MT_SUBTYPE, MF_PD_DURATION, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
    MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_D3D_MANAGER,
    MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READER_MEDIASOURCE, MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR, MFCreateAttributes,
    MFCreateDXGIDeviceManager, MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Video,
    MFVideoArea, MFVideoFormat_RGB32,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::core::{GUID, HSTRING, Interface};

const HNS: f64 = 10_000_000.0;
/// A wanted frame within this much after the current position is reached by
/// decoding on rather than seeking (an IDR frame comes every second).
const READ_ON: i64 = 15_000_000;
/// Timestamps closer than this count as equal.
const EPSILON: i64 = 10_000;

const VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// A decoded picture: top-down BGRA rows of `width * 4` bytes. The alpha
/// byte is undefined.
#[derive(Clone)]
pub struct Frame {
    /// Clip time of the frame.
    pub t: f64,
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Frame({:.3} s, {}x{})", self.t, self.width, self.height)
    }
}

#[derive(Debug)]
pub struct Decoder {
    reader: IMFSourceReader,
    /// Size of the reader's output and the part of it that is picture.
    size: (u32, u32),
    crop: Crop,
    /// The reader scales to the requested size.
    scaled: bool,
    /// Media time of the clip's first frame (its t = 0).
    base: i64,
    /// Seconds, from the container.
    pub duration: f64,
    /// The next sample, read but not returned yet.
    held: Option<(i64, IMFSample)>,
    /// Decoding runs on the GPU.
    pub hardware: bool,
    _gpu: Option<(ID3D11Device, IMFDXGIDeviceManager)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Crop {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl Decoder {
    /// Opens a clip. With `width`, frames come out scaled to that width (the
    /// height keeps the picture's aspect) if the video processor can; check
    /// [`Frame::width`].
    pub fn open(path: &Path, width: Option<u32>) -> Result<Self, String> {
        let path = std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))?;
        match Self::open_with(&path, true, width) {
            Ok(d) => Ok(d),
            Err(gpu) => Self::open_with(&path, false, width).map_err(|cpu| {
                format!(
                    "can't decode {}: {cpu} (with the GPU: {gpu})",
                    path.display()
                )
            }),
        }
    }

    fn open_with(path: &Path, hardware: bool, width: Option<u32>) -> Result<Self, String> {
        // SAFETY: Media Foundation calls on interfaces this function owns;
        // every out-pointer is a valid local.
        unsafe {
            let mut attrs: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attrs, 4).map_err(ctx("MFCreateAttributes"))?;
            let attrs = attrs.ok_or("MFCreateAttributes returned nothing")?;
            let gpu = if hardware {
                let device = create_device()?;
                let mut token = 0;
                let mut manager: Option<IMFDXGIDeviceManager> = None;
                MFCreateDXGIDeviceManager(&mut token, &mut manager)
                    .map_err(ctx("MFCreateDXGIDeviceManager"))?;
                let manager = manager.ok_or("MFCreateDXGIDeviceManager returned nothing")?;
                manager
                    .ResetDevice(&device, token)
                    .map_err(ctx("ResetDevice"))?;
                attrs
                    .SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &manager)
                    .map_err(ctx("MF_SOURCE_READER_D3D_MANAGER"))?;
                attrs
                    .SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)
                    .map_err(ctx("MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS"))?;
                Some((device, manager))
            } else {
                None
            };
            attrs
                .SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)
                .map_err(ctx("MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING"))?;
            let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attrs)
                .map_err(ctx("MFCreateSourceReaderFromURL"))?;
            reader
                .SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)
                .map_err(ctx("SetStreamSelection"))?;
            reader
                .SetStreamSelection(VIDEO, true)
                .map_err(|_| "the file has no video stream".to_owned())?;
            let native = reader
                .GetNativeMediaType(VIDEO, 0)
                .map_err(ctx("GetNativeMediaType"))?;
            let native_size = frame_size(&native)?;
            let native_crop = aperture(&native, native_size);
            // Scale only down, keeping the picture's aspect, to even sizes.
            let target = width.filter(|&w| w < native_crop.width).map(|w| {
                let w = w.max(16) & !1;
                let h = (u64::from(w) * u64::from(native_crop.height)
                    / u64::from(native_crop.width))
                .max(16) as u32
                    & !1;
                (w, h)
            });
            let scaled = match target {
                Some(size) => set_output(&reader, Some(size)).is_ok(),
                None => false,
            };
            if !scaled {
                set_output(&reader, None)?;
            }
            let current = reader
                .GetCurrentMediaType(VIDEO)
                .map_err(ctx("GetCurrentMediaType"))?;
            let size = frame_size(&current)?;
            let crop = if scaled {
                full(size)
            } else if size == native_size {
                let own = aperture(&current, size);
                if own == full(size) { native_crop } else { own }
            } else {
                aperture(&current, size)
            };
            let duration = reader
                .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
                .ok()
                .and_then(|v| u64::try_from(&v).ok())
                .map_or(0.0, |d| d as f64 / HNS);
            let mut decoder = Self {
                reader,
                size,
                crop,
                scaled,
                base: 0,
                duration,
                held: None,
                hardware,
                _gpu: gpu,
            };
            decoder.held = decoder.read()?;
            let (base, _) = decoder.held.as_ref().ok_or("the video has no frames")?;
            decoder.base = *base;
            // Read the first frame now, so a decoder that can't actually
            // produce pictures (a missing codec) fails here and the caller
            // can fall back.
            decoder.frame_at(0.0)?;
            Ok(decoder)
        }
    }

    /// Whether frames come out at the requested width.
    pub fn scaled(&self) -> bool {
        self.scaled
    }

    /// The frame shown at clip time `t`: the last one starting at or before
    /// it. Past the end, the last frame.
    pub fn frame_at(&mut self, t: f64) -> Result<Frame, String> {
        let target = self.base + (t.max(0.0) * HNS).round() as i64;
        let near = (self.held.as_ref())
            .is_some_and(|(at, _)| *at <= target + EPSILON && target - at < READ_ON);
        if !near {
            self.seek(target)?;
        }
        loop {
            let Some((at, sample)) = self.held.take() else {
                return Err(format!("no frame at {t:.3} s"));
            };
            if at > target + EPSILON {
                // Only after a seek that landed late; this is the nearest.
                let frame = self.lock(&sample, at);
                self.held = Some((at, sample));
                return frame;
            }
            match self.read()? {
                Some((next_at, next)) if next_at <= target + EPSILON => {
                    self.held = Some((next_at, next));
                }
                Some(next) => {
                    let frame = self.lock(&sample, at);
                    self.held = Some(next);
                    return frame;
                }
                None => {
                    // The end: keep the last frame for later requests.
                    let frame = self.lock(&sample, at);
                    self.held = Some((at, sample));
                    return frame;
                }
            }
        }
    }

    /// Calls `f` with one frame every `step` seconds, decoding the clip
    /// once from start to end. Stops early if `f` returns false.
    pub fn each(&mut self, step: f64, mut f: impl FnMut(Frame) -> bool) -> Result<(), String> {
        let step = (step.max(0.001) * HNS).round() as i64;
        self.seek(self.base)?;
        let mut next_slot = self.base;
        while let Some((at, sample)) = self.held.take() {
            if at + EPSILON >= next_slot {
                let frame = self.lock(&sample, at)?;
                if !f(frame) {
                    return Ok(());
                }
                while next_slot <= at + EPSILON {
                    next_slot += step;
                }
            }
            self.held = self.read()?;
        }
        Ok(())
    }

    fn seek(&mut self, target: i64) -> Result<(), String> {
        self.seek_to(target)?;
        // The reader seeks to the key frame before the target; if this one
        // landed after it, back off once.
        if let Some((at, _)) = &self.held
            && *at > target + EPSILON
            && target > self.base
        {
            self.seek_to((target - 11_000_000).max(self.base))?;
        }
        Ok(())
    }

    fn seek_to(&mut self, target: i64) -> Result<(), String> {
        self.held = None;
        // Past the end is MF_E_INVALID_POSITION; reading on from just before
        // it gives the last frame.
        let end = self.base + (self.duration * HNS) as i64 - EPSILON;
        let target = if self.duration > 0.0 {
            target.min(end)
        } else {
            target
        };
        let position = PROPVARIANT::from(target.max(0));
        // SAFETY: GUID_NULL means 100 ns units; `position` outlives the call.
        unsafe { self.reader.SetCurrentPosition(&GUID::zeroed(), &position) }
            .map_err(ctx("SetCurrentPosition"))?;
        self.held = self.read()?;
        Ok(())
    }

    /// The next decoded sample, or `None` at the end of the stream.
    fn read(&mut self) -> Result<Option<(i64, IMFSample)>, String> {
        loop {
            let mut flags = 0u32;
            let mut at = 0i64;
            let mut sample: Option<IMFSample> = None;
            // SAFETY: all out-pointers are valid locals.
            unsafe {
                self.reader.ReadSample(
                    VIDEO,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut at),
                    Some(&mut sample),
                )
            }
            .map_err(ctx("ReadSample"))?;
            let flags = flags as i32;
            if flags & MF_SOURCE_READERF_ERROR.0 != 0 {
                return Err("the decoder failed".into());
            }
            if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 != 0 {
                // SAFETY: plain query on a valid reader.
                let current = unsafe { self.reader.GetCurrentMediaType(VIDEO) }
                    .map_err(ctx("GetCurrentMediaType"))?;
                self.size = frame_size(&current)?;
                self.crop = if self.scaled {
                    full(self.size)
                } else {
                    aperture(&current, self.size)
                };
            }
            if let Some(sample) = sample {
                return Ok(Some((at, sample)));
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 != 0 {
                return Ok(None);
            }
            // A stream tick or gap: read on.
        }
    }

    /// Copies a sample's picture out.
    fn lock(&self, sample: &IMFSample, at: i64) -> Result<Frame, String> {
        let Crop {
            x,
            y,
            width,
            height,
        } = self.crop;
        let row = width as usize * 4;
        let mut bgra = vec![0u8; row * height as usize];
        // SAFETY: the buffer is locked while its memory is read, the rows
        // read lie inside the locked picture (`crop` is within `size`), and
        // it is unlocked on every path.
        unsafe {
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(ctx("ConvertToContiguousBuffer"))?;
            let mut copy = |scan0: *const u8, pitch: isize| {
                for r in 0..height as usize {
                    let src = scan0
                        .offset((y as usize + r) as isize * pitch)
                        .add(x as usize * 4);
                    std::ptr::copy_nonoverlapping(src, bgra.as_mut_ptr().add(r * row), row);
                }
            };
            if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
                let mut scan0 = std::ptr::null_mut();
                let mut pitch = 0i32;
                two_d
                    .Lock2D(&mut scan0, &mut pitch)
                    .map_err(ctx("Lock2D"))?;
                copy(scan0, pitch as isize);
                let _ = two_d.Unlock2D();
            } else {
                let mut data = std::ptr::null_mut();
                let mut len = 0u32;
                buffer
                    .Lock(&mut data, None, Some(&mut len))
                    .map_err(ctx("Lock"))?;
                let pitch = self.stride();
                let needed = (y + height) as usize * pitch.unsigned_abs();
                if (len as usize) < needed || pitch < 0 {
                    let _ = buffer.Unlock();
                    return Err("unexpected frame buffer layout".into());
                }
                copy(data, pitch);
                let _ = buffer.Unlock();
            }
        }
        Ok(Frame {
            t: ((at - self.base) as f64 / HNS).max(0.0),
            width,
            height,
            bgra,
        })
    }

    fn stride(&self) -> isize {
        // SAFETY: plain queries on a valid reader and media type.
        let stride = unsafe {
            self.reader
                .GetCurrentMediaType(VIDEO)
                .ok()
                .and_then(|t| t.GetUINT32(&MF_MT_DEFAULT_STRIDE).ok())
        };
        stride.map_or(self.size.0 as isize * 4, |s| s as i32 as isize)
    }
}

/// Asks the reader for BGRA, at `size` if given.
fn set_output(reader: &IMFSourceReader, size: Option<(u32, u32)>) -> Result<(), String> {
    // SAFETY: a new media type set up and handed to a valid reader.
    unsafe {
        let wanted = MFCreateMediaType().map_err(ctx("MFCreateMediaType"))?;
        wanted
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(ctx("MF_MT_MAJOR_TYPE"))?;
        wanted
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
            .map_err(ctx("MF_MT_SUBTYPE"))?;
        if let Some((w, h)) = size {
            wanted
                .SetUINT64(&MF_MT_FRAME_SIZE, u64::from(w) << 32 | u64::from(h))
                .map_err(ctx("MF_MT_FRAME_SIZE"))?;
        }
        reader
            .SetCurrentMediaType(VIDEO, None, &wanted)
            .map_err(ctx("the video processor can't make BGRA frames"))?;
        if let Some(size) = size {
            let current = reader
                .GetCurrentMediaType(VIDEO)
                .map_err(ctx("GetCurrentMediaType"))?;
            if frame_size(&current)? != size {
                return Err("the video processor did not scale".into());
            }
        }
    }
    Ok(())
}

fn frame_size(media: &IMFMediaType) -> Result<(u32, u32), String> {
    // SAFETY: plain query on a valid media type.
    let packed = unsafe { media.GetUINT64(&MF_MT_FRAME_SIZE) }.map_err(ctx("MF_MT_FRAME_SIZE"))?;
    let size = ((packed >> 32) as u32, packed as u32);
    if size.0 == 0 || size.1 == 0 {
        return Err("the video has no frame size".into());
    }
    Ok(size)
}

/// The visible part of the picture (`MF_MT_MINIMUM_DISPLAY_APERTURE`), or
/// all of it.
fn aperture(media: &IMFMediaType, size: (u32, u32)) -> Crop {
    let mut area = MFVideoArea::default();
    // SAFETY: the blob is copied into `area`, whose size is passed.
    let read = unsafe {
        media.GetBlob(
            &MF_MT_MINIMUM_DISPLAY_APERTURE,
            std::slice::from_raw_parts_mut((&raw mut area).cast::<u8>(), size_of::<MFVideoArea>()),
            None,
        )
    };
    if read.is_err() {
        return full(size);
    }
    let x = area.OffsetX.value.max(0) as u32;
    let y = area.OffsetY.value.max(0) as u32;
    let (w, h) = (area.Area.cx.max(0) as u32, area.Area.cy.max(0) as u32);
    if w == 0 || h == 0 || x + w > size.0 || y + h > size.1 {
        return full(size);
    }
    Crop {
        x,
        y,
        width: w,
        height: h,
    }
}

fn full(size: (u32, u32)) -> Crop {
    Crop {
        x: 0,
        y: 0,
        width: size.0,
        height: size.1,
    }
}

fn create_device() -> Result<ID3D11Device, String> {
    let mut device = None;
    // SAFETY: standard device creation with valid out-pointers.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }
    .map_err(ctx("D3D11CreateDevice"))?;
    let device: ID3D11Device = device.ok_or("D3D11CreateDevice returned no device")?;
    // Media Foundation uses the device from its own threads.
    if let Ok(multithread) = device.cast::<ID3D11Multithread>() {
        // SAFETY: simple flag set on a valid interface.
        let _ = unsafe { multithread.SetMultithreadProtected(true) };
    }
    Ok(device)
}

fn ctx(what: &'static str) -> impl Fn(windows::core::Error) -> String {
    move |e| format!("{what}: {e}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes a real clip: `REC_TEST_CLIP=<clip.mp4> cargo test -p
    /// rec-library decode -- --ignored`.
    #[test]
    #[ignore = "needs a clip in REC_TEST_CLIP and a video decoder"]
    fn decodes_a_clip() {
        crate::sys::init().unwrap();
        let path = std::env::var_os("REC_TEST_CLIP").expect("set REC_TEST_CLIP");
        let mut d = Decoder::open(Path::new(&path), None).unwrap();
        assert!(d.duration > 0.5, "{}", d.duration);
        let first = d.frame_at(0.0).unwrap();
        assert_eq!(first.t, 0.0);
        let mid = d.frame_at(d.duration / 2.0).unwrap();
        assert!((mid.t - d.duration / 2.0).abs() < 0.05, "{mid:?}");
        // Backwards needs a seek.
        let early = d.frame_at(0.5).unwrap();
        assert!((early.t - 0.5).abs() < 0.05, "{early:?}");
        let end = d.frame_at(d.duration + 5.0).unwrap();
        // The last frame (the container duration may be rounded).
        assert!(
            (end.t - d.duration).abs() < 1.1,
            "{end:?} vs {}",
            d.duration
        );
        let mut n = 0;
        let started = std::time::Instant::now();
        d.each(0.5, |_| {
            n += 1;
            true
        })
        .unwrap();
        eprintln!(
            "hardware {}, full pass {:.2} s for {:.1} s of video",
            d.hardware,
            started.elapsed().as_secs_f64(),
            d.duration
        );
        assert!(n as f64 >= d.duration * 2.0 - 1.0, "{n}");
        let mut small = Decoder::open(Path::new(&path), Some(320)).unwrap();
        let f = small.frame_at(1.0).unwrap();
        assert_eq!(f.width, 320, "scaled: {}", small.scaled());
    }
}
