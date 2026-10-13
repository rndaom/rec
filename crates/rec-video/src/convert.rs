//! GPU colour conversion and scaling of the "latest" captured texture into
//! the encoder's input surfaces (see [`crate::pool`]):
//!
//! * SDR (BGRA8 sRGB → NV12 BT.709) runs on the D3D11 video processor,
//!   fixed-function on most GPUs.
//! * HDR captures (FP16 scRGB) go through our own shaders ([`crate::shader`])
//!   to P010 HDR10 or, tone-mapped, to NV12 BT.709: what video processors do
//!   with HDR input differs too much between drivers to rely on.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0,
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
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_RATIONAL,
};
use windows::Win32::Media::MediaFoundation::IMFSample;
use windows::core::Interface;

use crate::VideoError;
use crate::capture::{CaptureFormat, Latest};
use crate::geometry::place;
use crate::pool::Pool;
use crate::shader::ShaderConverter;
use crate::util::Context;

/// What the converter turns captured frames into.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Conversion {
    /// BGRA8 sRGB → NV12 BT.709.
    Sdr,
    /// FP16 scRGB → NV12 BT.709, highlights above SDR white rolled off.
    Tonemap {
        /// Brightness of SDR white on the display; it maps to 100%.
        sdr_white_nits: f32,
    },
    /// FP16 scRGB → P010 HDR10 (BT.2020, PQ), clipped at the display's peak.
    Hdr10 { peak_nits: f32 },
}

impl Conversion {
    pub(crate) fn capture_format(self) -> CaptureFormat {
        match self {
            Conversion::Sdr => CaptureFormat::Bgra8,
            Conversion::Tonemap { .. } | Conversion::Hdr10 { .. } => CaptureFormat::Fp16,
        }
    }

    pub(crate) fn output_format(self) -> DXGI_FORMAT {
        match self {
            Conversion::Hdr10 { .. } => DXGI_FORMAT_P010,
            Conversion::Sdr | Conversion::Tonemap { .. } => DXGI_FORMAT_NV12,
        }
    }
}

/// The encoder input surfaces plus whichever converter fills them.
pub(crate) struct Converter {
    pool: Pool,
    kind: Kind,
}

enum Kind {
    Video(VideoConverter),
    Shader(ShaderConverter),
}

impl std::fmt::Debug for Converter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Converter")
            .field("pool", &self.pool)
            .finish_non_exhaustive()
    }
}

impl Converter {
    /// Surfaces of `coded` size whose visible `picture` is centred in them
    /// (they differ only where the encoder needs padding).
    pub(crate) fn new(
        device: &ID3D11Device,
        coded: (u32, u32),
        picture: (u32, u32),
        fps: u32,
        conversion: Conversion,
    ) -> Result<Self, VideoError> {
        let pool = Pool::new(device, coded.0, coded.1, conversion.output_format())?;
        let kind = match conversion {
            Conversion::Sdr => Kind::Video(VideoConverter::new(device, &pool, picture, fps)?),
            other => Kind::Shader(ShaderConverter::new(device, &pool, picture, other)?),
        };
        Ok(Self { pool, kind })
    }

    /// True if `slot`'s surface is back from the encoder.
    pub(crate) fn is_free(&self, slot: usize) -> bool {
        self.pool.is_free(slot)
    }

    /// A free slot, preferring one other than `avoid` (the last submitted).
    pub(crate) fn free_slot(&self, avoid: Option<usize>) -> Option<usize> {
        self.pool.free_slot(avoid)
    }

    /// `slot`'s surface (output size and format).
    pub(crate) fn texture(&self, slot: usize) -> &ID3D11Texture2D {
        self.pool.texture(slot)
    }

    /// Takes `slot`'s sample for submission; see [`Pool::take_sample`].
    pub(crate) fn take_sample(&self, slot: usize) -> Result<IMFSample, VideoError> {
        self.pool.take_sample(slot)
    }

    /// Converts `latest` into `slot`'s surface.
    pub(crate) fn blit(&mut self, latest: &Latest, slot: usize) -> Result<(), VideoError> {
        match &mut self.kind {
            Kind::Video(v) => v.blit(latest, &self.pool, slot),
            Kind::Shader(s) => s.blit(latest, &self.pool, slot),
        }
    }
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
    /// One per pool slot.
    output_views: Vec<ID3D11VideoProcessorOutputView>,
    /// Content size the stream rects were last set for.
    rect_for: (u32, u32),
}

/// BGRA8 → NV12 on the D3D11 video processor.
struct VideoConverter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    width: u32,
    height: u32,
    /// Visible part of the surfaces; see [`place`].
    picture: (u32, u32),
    fps: u32,
    processor: Option<Processor>,
}

impl VideoConverter {
    fn new(
        device: &ID3D11Device,
        pool: &Pool,
        picture: (u32, u32),
        fps: u32,
    ) -> Result<Self, VideoError> {
        let (width, height) = (pool.width, pool.height);
        let video_device: ID3D11VideoDevice = device.cast().ctx("ID3D11VideoDevice")?;
        // SAFETY: plain getter on a valid device.
        let context = unsafe { device.GetImmediateContext() }.ctx("GetImmediateContext")?;
        let video_context: ID3D11VideoContext = context.cast().ctx("ID3D11VideoContext")?;

        let converter = Self {
            video_device,
            video_context,
            width,
            height,
            picture,
            fps,
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
    fn prepare(
        &mut self,
        texture: &ID3D11Texture2D,
        w: u32,
        h: u32,
        pool: &Pool,
    ) -> Result<(), VideoError> {
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
        let mut output_views = Vec::with_capacity(pool.len());
        for slot in 0..pool.len() {
            let mut view = None;
            // SAFETY: NV12 render-target texture and valid enumerator.
            unsafe {
                self.video_device.CreateVideoProcessorOutputView(
                    pool.texture(slot),
                    &enumerator,
                    &out_desc,
                    Some(&mut view),
                )
            }
            .ctx("CreateVideoProcessorOutputView")?;
            output_views.push(view.ok_or_else(|| {
                VideoError::Other("CreateVideoProcessorOutputView returned null".into())
            })?);
        }

        self.configure(&processor);
        self.processor = Some(Processor {
            input_texture: texture.clone(),
            input_width: w,
            input_height: h,
            _enumerator: enumerator,
            processor,
            input_view,
            output_views,
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

    /// Converts `latest` into `slot`'s NV12 surface.
    fn blit(&mut self, latest: &Latest, pool: &Pool, slot: usize) -> Result<(), VideoError> {
        let Some(texture) = latest.texture.clone() else {
            return Ok(());
        };
        self.prepare(&texture, latest.tex_width, latest.tex_height, pool)?;
        let Some(p) = self.processor.as_mut() else {
            return Ok(());
        };
        let out_view = &p.output_views[slot];
        let vc = &self.video_context;
        let content = (latest.content_width, latest.content_height);
        if p.rect_for != content {
            let src = RECT {
                left: 0,
                top: 0,
                right: content.0 as i32,
                bottom: content.1 as i32,
            };
            let r = place(
                content.0,
                content.1,
                self.picture,
                (self.width, self.height),
            );
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
}
