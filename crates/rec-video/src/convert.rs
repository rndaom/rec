//! GPU colour conversion and scaling: BGRA "latest" texture → NV12 encoder
//! input, via the D3D11 video processor (fixed-function on most GPUs).
//!
//! The NV12 surfaces live in a small pool. Each is wrapped once in an
//! `IMFTrackedSample` around an `MFCreateDXGISurfaceBuffer` (zero copy). When
//! the encoder releases a sample, the tracked sample calls back and the slot
//! becomes free again, so a surface is never overwritten while the encoder
//! still reads it.

use std::mem::ManuallyDrop;
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{E_NOTIMPL, RECT};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0,
    D3D11_VIDEO_COLOR_RGBA, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device, ID3D11Texture2D,
    ID3D11VideoContext, ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Media::MediaFoundation::{
    IMFAsyncCallback, IMFAsyncCallback_Impl, IMFAsyncResult, IMFSample, IMFTrackedSample,
    MFCreateDXGISurfaceBuffer, MFCreateTrackedSample,
};
use windows::core::{Interface, Ref, implement};

use crate::VideoError;
use crate::capture::Latest;
use crate::geometry::letterbox;
use crate::util::Context;

/// NV12 surfaces in flight between the converter and the encoder.
pub(crate) const POOL_SIZE: usize = 4;

/// An `IMFSample` that crosses to the MF thread that releases it.
struct SendSample(IMFSample);
// SAFETY: Media Foundation samples are free-threaded objects; ownership is
// handed over whole (no shared aliasing), guarded by the `FreeList` mutex.
unsafe impl Send for SendSample {}

/// Samples the encoder has released, indexed by slot.
#[derive(Default)]
struct FreeList(Mutex<Vec<Option<SendSample>>>);

impl FreeList {
    fn put(&self, slot: usize, sample: IMFSample) {
        if let Ok(mut slots) = self.0.lock() {
            if let Some(entry) = slots.get_mut(slot) {
                *entry = Some(SendSample(sample));
            }
        }
    }

    fn take(&self, slot: usize) -> Option<IMFSample> {
        self.0.lock().ok()?.get_mut(slot)?.take().map(|s| s.0)
    }

    fn is_free(&self, slot: usize) -> bool {
        self.0
            .lock()
            .ok()
            .is_some_and(|slots| slots.get(slot).is_some_and(Option::is_some))
    }

    fn any_free(&self, exclude: Option<usize>) -> Option<usize> {
        let slots = self.0.lock().ok()?;
        slots
            .iter()
            .enumerate()
            .find(|(i, s)| s.is_some() && Some(*i) != exclude)
            .map(|(i, _)| i)
    }

    fn clear(&self) {
        // Drop outside the lock: releasing a sample may re-enter `put`.
        let drained: Vec<Option<SendSample>> = match self.0.lock() {
            Ok(mut slots) => slots.iter_mut().map(Option::take).collect(),
            Err(_) => return,
        };
        drop(drained);
    }
}

/// Tracked-sample callback: the encoder dropped its last reference.
#[implement(IMFAsyncCallback)]
struct SampleReturned {
    slot: usize,
    free: Arc<FreeList>,
}

impl IMFAsyncCallback_Impl for SampleReturned_Impl {
    fn GetParameters(&self, _flags: *mut u32, _queue: *mut u32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Invoke(&self, result: Ref<IMFAsyncResult>) -> windows::core::Result<()> {
        let result = result.ok()?;
        // SAFETY: the tracked sample passes itself as the result object.
        let object = unsafe { result.GetObject() }?;
        self.free.put(self.slot, object.cast()?);
        Ok(())
    }
}

struct Slot {
    texture: ID3D11Texture2D,
    view: Option<ID3D11VideoProcessorOutputView>,
    callback: IMFAsyncCallback,
}

/// Video processor state for one input texture size.
struct Processor {
    input_texture: ID3D11Texture2D,
    input_width: u32,
    input_height: u32,
    /// Kept alive for the views created from it.
    _enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input_view: ID3D11VideoProcessorInputView,
    /// Content size the stream rects were last set for.
    rect_for: (u32, u32),
}

pub(crate) struct Converter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    width: u32,
    height: u32,
    fps: u32,
    slots: Vec<Slot>,
    free: Arc<FreeList>,
    processor: Option<Processor>,
}

impl std::fmt::Debug for Converter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Converter")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl Converter {
    pub(crate) fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Self, VideoError> {
        let video_device: ID3D11VideoDevice = device.cast().ctx("ID3D11VideoDevice")?;
        // SAFETY: plain getter on a valid device.
        let context = unsafe { device.GetImmediateContext() }.ctx("GetImmediateContext")?;
        let video_context: ID3D11VideoContext = context.cast().ctx("ID3D11VideoContext")?;

        let free = Arc::new(FreeList::default());
        let mut slots = Vec::with_capacity(POOL_SIZE);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut samples = Vec::with_capacity(POOL_SIZE);
        for slot in 0..POOL_SIZE {
            let mut texture = None;
            // SAFETY: valid descriptor; out-pointer valid for writes.
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
                .ctx("CreateTexture2D(NV12)")?;
            let texture =
                texture.ok_or_else(|| VideoError::Other("CreateTexture2D returned null".into()))?;
            // SAFETY: wraps subresource 0 of a valid texture; no CPU copy.
            let buffer =
                unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &texture, 0, false) }
                    .ctx("MFCreateDXGISurfaceBuffer")?;
            // SAFETY: plain factory call.
            let tracked: IMFTrackedSample =
                unsafe { MFCreateTrackedSample() }.ctx("MFCreateTrackedSample")?;
            let sample: IMFSample = tracked.cast().ctx("IMFSample")?;
            // SAFETY: valid buffer.
            unsafe { sample.AddBuffer(&buffer) }.ctx("IMFSample::AddBuffer")?;
            samples.push(SendSample(sample));
            let callback: IMFAsyncCallback = SampleReturned {
                slot,
                free: free.clone(),
            }
            .into();
            slots.push(Slot {
                texture,
                view: None,
                callback,
            });
        }
        *free
            .0
            .lock()
            .map_err(|_| VideoError::Other("lock poisoned".into()))? =
            samples.into_iter().map(Some).collect();

        let converter = Self {
            video_device,
            video_context,
            width,
            height,
            fps,
            slots,
            free,
            processor: None,
        };
        converter.check_formats()?;
        Ok(converter)
    }

    fn check_formats(&self) -> Result<(), VideoError> {
        // A throwaway enumerator just to ask about format support.
        let enumerator = self.enumerator(self.width, self.height)?;
        let support = |f: DXGI_FORMAT| {
            // SAFETY: plain capability query.
            unsafe { enumerator.CheckVideoProcessorFormat(f) }.unwrap_or(0)
        };
        if support(DXGI_FORMAT_B8G8R8A8_UNORM) & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32
            == 0
            || support(DXGI_FORMAT_NV12) & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0
        {
            return Err(VideoError::Other(
                "the GPU video processor cannot convert BGRA to NV12".into(),
            ));
        }
        Ok(())
    }

    fn enumerator(
        &self,
        input_width: u32,
        input_height: u32,
    ) -> Result<ID3D11VideoProcessorEnumerator, VideoError> {
        let rate = DXGI_RATIONAL {
            Numerator: self.fps,
            Denominator: 1,
        };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: input_width,
            InputHeight: input_height,
            OutputFrameRate: rate,
            OutputWidth: self.width,
            OutputHeight: self.height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        // SAFETY: valid descriptor.
        unsafe { self.video_device.CreateVideoProcessorEnumerator(&desc) }
            .ctx("CreateVideoProcessorEnumerator")
    }

    /// (Re)creates the processor and views when the input texture changes.
    fn prepare(&mut self, texture: &ID3D11Texture2D, w: u32, h: u32) -> Result<(), VideoError> {
        let current = self.processor.as_ref().is_some_and(|p| {
            p.input_texture == *texture && p.input_width == w && p.input_height == h
        });
        if current {
            return Ok(());
        }
        self.processor = None;
        let enumerator = self.enumerator(w, h)?;
        // SAFETY: valid enumerator; rate conversion index 0 always exists.
        let processor = unsafe { self.video_device.CreateVideoProcessor(&enumerator, 0) }
            .ctx("CreateVideoProcessor")?;

        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut input_view = None;
        // SAFETY: texture and enumerator are valid; out-pointer valid.
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                texture,
                &enumerator,
                &in_desc,
                Some(&mut input_view),
            )
        }
        .ctx("CreateVideoProcessorInputView")?;
        let input_view = input_view.ok_or_else(|| {
            VideoError::Other("CreateVideoProcessorInputView returned null".into())
        })?;

        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        for slot in &mut self.slots {
            let mut view = None;
            // SAFETY: NV12 render-target texture and valid enumerator.
            unsafe {
                self.video_device.CreateVideoProcessorOutputView(
                    &slot.texture,
                    &enumerator,
                    &out_desc,
                    Some(&mut view),
                )
            }
            .ctx("CreateVideoProcessorOutputView")?;
            slot.view = view;
        }

        self.configure(&processor);
        self.processor = Some(Processor {
            input_texture: texture.clone(),
            input_width: w,
            input_height: h,
            _enumerator: enumerator,
            processor,
            input_view,
            rect_for: (0, 0),
        });
        Ok(())
    }

    /// Static processor state: colour spaces, background, no auto processing.
    fn configure(&self, processor: &ID3D11VideoProcessor) {
        let vc = &self.video_context;
        let full = RECT {
            left: 0,
            top: 0,
            right: self.width as i32,
            bottom: self.height as i32,
        };
        let black = D3D11_VIDEO_COLOR {
            Anonymous: D3D11_VIDEO_COLOR_0 {
                RGBA: D3D11_VIDEO_COLOR_RGBA {
                    R: 0.0,
                    G: 0.0,
                    B: 0.0,
                    A: 1.0,
                },
            },
        };
        // SAFETY: all calls only set state on a valid processor; pointers are
        // to locals that outlive the calls.
        unsafe {
            vc.VideoProcessorSetStreamFrameFormat(
                processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            vc.VideoProcessorSetStreamAutoProcessingMode(processor, 0, false);
            vc.VideoProcessorSetOutputTargetRect(processor, true, Some(&full));
            vc.VideoProcessorSetOutputBackgroundColor(processor, false, &black);
            if let Ok(vc1) = vc.cast::<ID3D11VideoContext1>() {
                // sRGB desktop (full-range RGB, BT.709 primaries) → BT.709
                // limited-range YCbCr, the SDR broadcast norm players expect.
                vc1.VideoProcessorSetStreamColorSpace1(
                    processor,
                    0,
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                );
                vc1.VideoProcessorSetOutputColorSpace1(
                    processor,
                    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                );
            } else {
                // Bitfield: Usage:1 RGB_Range:1 YCbCr_Matrix:1 YCbCr_xvYCC:1
                // Nominal_Range:2. Input: full-range RGB (all 0). Output:
                // BT.709 matrix, 16-235.
                let input = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
                let output = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: (1 << 2) | (1 << 4),
                };
                vc.VideoProcessorSetStreamColorSpace(processor, 0, &input);
                vc.VideoProcessorSetOutputColorSpace(processor, &output);
            }
        }
    }

    /// True if `slot`'s sample is back from the encoder.
    pub(crate) fn is_free(&self, slot: usize) -> bool {
        self.free.is_free(slot)
    }

    /// A free slot, preferring one other than `avoid` (the last submitted).
    pub(crate) fn free_slot(&self, avoid: Option<usize>) -> Option<usize> {
        self.free
            .any_free(avoid)
            .or_else(|| self.free.any_free(None))
    }

    /// Converts `latest` into `slot`'s NV12 surface.
    pub(crate) fn blit(&mut self, latest: &Latest, slot: usize) -> Result<(), VideoError> {
        let Some(texture) = latest.texture.clone() else {
            return Ok(());
        };
        self.prepare(&texture, latest.tex_width, latest.tex_height)?;
        let (Some(p), Some(out_view)) = (self.processor.as_mut(), self.slots[slot].view.as_ref())
        else {
            return Ok(());
        };
        let vc = &self.video_context;
        let content = (latest.content_width, latest.content_height);
        if p.rect_for != content {
            let src = RECT {
                left: 0,
                top: 0,
                right: content.0 as i32,
                bottom: content.1 as i32,
            };
            let r = letterbox(content.0, content.1, self.width, self.height);
            let dst = RECT {
                left: r.left as i32,
                top: r.top as i32,
                right: r.right as i32,
                bottom: r.bottom as i32,
            };
            // SAFETY: state setters on a valid processor; rects outlive calls.
            unsafe {
                vc.VideoProcessorSetStreamSourceRect(&p.processor, 0, true, Some(&src));
                vc.VideoProcessorSetStreamDestRect(&p.processor, 0, true, Some(&dst));
            }
            p.rect_for = content;
        }
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(p.input_view.clone())),
            ..Default::default()
        };
        // SAFETY: valid processor and views; `stream` lives across the call.
        let result = unsafe {
            vc.VideoProcessorBlt(&p.processor, out_view, 0, std::slice::from_ref(&stream))
        };
        // SAFETY: releases the reference we put into the stream descriptor,
        // exactly once.
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result.ctx("VideoProcessorBlt")
    }

    /// Takes `slot`'s sample for submission and arms its return callback.
    pub(crate) fn take_sample(&self, slot: usize) -> Result<IMFSample, VideoError> {
        let sample = self
            .free
            .take(slot)
            .ok_or_else(|| VideoError::Other(format!("NV12 slot {slot} is not free")))?;
        let tracked: IMFTrackedSample = sample.cast().ctx("IMFTrackedSample")?;
        // SAFETY: valid callback; the sample calls it once when released.
        unsafe { tracked.SetAllocator(&self.slots[slot].callback, None) }
            .ctx("IMFTrackedSample::SetAllocator")?;
        Ok(sample)
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        // Break the sample → callback → free list → sample cycle.
        self.free.clear();
    }
}
