//! FP16 scRGB captures → encoder input, on our own shaders: P010 HDR10 or a
//! tone-mapped NV12 SDR picture.
//!
//! Windows composes an HDR desktop in scRGB: linear light, BT.709 primaries,
//! 1.0 = 80 cd/m², values above 1 for highlights and below 0 for colours
//! outside BT.709. Two passes write the planes of the planar output texture
//! directly (a render-target view per plane): luma at full size, then chroma
//! at half size. Both sample the capture bilinearly, so scaling and the
//! letterbox happen here too.
//!
//! * **HDR10:** BT.709 → BT.2020 primaries, clip to the display's peak (the
//!   picture never claims more than the display could show, which keeps the
//!   HDR10 metadata honest), PQ, BT.2020 non-constant-luminance Y'CbCr,
//!   10-bit limited range.
//! * **Tone-map:** SDR white (Windows' "SDR content brightness") becomes
//!   100%; highlights above a knee roll off exponentially towards 100%
//!   instead of clipping, per pixel on the brightest channel so hues hold;
//!   then the sRGB curve and BT.709 Y'CbCr, 8-bit limited range, the same
//!   signal the SDR video-processor path produces.
//!
//! Chroma is left-sited (H.273 type 0, the default every decoder assumes):
//! each sample is a [1 2 1]/4 horizontal by [1 1]/2 vertical average of the
//! non-linear R'G'B' around it, which keeps fine coloured detail (text, HUD
//! edges) from aliasing.
//!
//! The shaders are compiled at pipeline start by `d3dcompiler_47.dll`, which
//! ships with Windows 10 and 11 (about 20 ms); nothing is bundled.

use std::ffi::CString;

use windows::Win32::Graphics::Direct3D::Fxc::{D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile};
use windows::Win32::Graphics::Direct3D::{
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SHADER_MACRO, D3D11_SRV_DIMENSION_TEXTURE2D, ID3DBlob,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BUFFER_DESC, D3D11_COMPARISON_NEVER,
    D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_FORMAT_SUPPORT_RENDER_TARGET,
    D3D11_RENDER_TARGET_VIEW_DESC, D3D11_RENDER_TARGET_VIEW_DESC_0, D3D11_RTV_DIMENSION_TEXTURE2D,
    D3D11_SAMPLER_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0,
    D3D11_TEX2D_RTV, D3D11_TEX2D_SRV, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT,
    D3D11_VIEWPORT, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11PixelShader, ID3D11RenderTargetView, ID3D11SamplerState, ID3D11ShaderResourceView,
    ID3D11Texture2D, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM,
    DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16G16_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
};
use windows::core::{Interface, PCSTR};

use crate::VideoError;
use crate::capture::Latest;
use crate::convert::Conversion;
use crate::geometry::place;
use crate::pool::Pool;
use crate::util::Context;

const SOURCE: &str = include_str!("convert.hlsl");

/// Constant buffer; layout matches `Params` in `convert.hlsl` (16-byte rows).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Params {
    dst_origin: [f32; 2],
    dst_size: [f32; 2],
    src_size: [f32; 2],
    tex_size_inv: [f32; 2],
    /// scRGB → working units: cd/m² for HDR10, fraction of SDR white for
    /// the tone-map.
    scale: f32,
    /// HDR10: clip level in cd/m².
    peak: f32,
    _pad: [f32; 2],
}

/// Render-target views of one output surface's two planes.
struct Planes {
    luma: ID3D11RenderTargetView,
    chroma: ID3D11RenderTargetView,
}

/// The current capture texture and its view.
struct Input {
    texture: ID3D11Texture2D,
    view: ID3D11ShaderResourceView,
}

pub(crate) struct ShaderConverter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    multithread: ID3D11Multithread,
    vertex: ID3D11VertexShader,
    luma: ID3D11PixelShader,
    chroma: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    constants: ID3D11Buffer,
    planes: Vec<Planes>,
    input: Option<Input>,
    /// What `constants` holds, to skip the upload when nothing changed.
    params: Option<Params>,
    width: u32,
    height: u32,
    /// Visible part of the surfaces; see [`place`].
    picture: (u32, u32),
    /// `Params::scale` and `Params::peak` for this conversion.
    scale: f32,
    peak: f32,
}

impl ShaderConverter {
    pub(crate) fn new(
        device: &ID3D11Device,
        pool: &Pool,
        picture: (u32, u32),
        conversion: Conversion,
    ) -> Result<Self, VideoError> {
        let (hdr10, scale, peak) = match conversion {
            Conversion::Hdr10 { peak_nits } => (true, 80.0, peak_nits.max(80.0)),
            Conversion::Tonemap { sdr_white_nits } => (false, 80.0 / sdr_white_nits.max(1.0), 1.0),
            Conversion::Sdr => {
                return Err(VideoError::Other(
                    "the shader converter takes FP16 input only".into(),
                ));
            }
        };
        let (luma_format, chroma_format) = plane_formats(pool.format)?;
        check_render_target(device, pool.format)?;

        let defines: &[(&str, &str)] = if hdr10 { &[("HDR10", "1")] } else { &[] };
        let vs = compile("vs_main", "vs_5_0", defines)?;
        let ps_luma = compile("ps_luma", "ps_5_0", defines)?;
        let ps_chroma = compile("ps_chroma", "ps_5_0", defines)?;

        let mut vertex = None;
        let mut luma = None;
        let mut chroma = None;
        // SAFETY: the bytecode blobs are valid compiled shaders; out-pointers
        // are valid for writes.
        unsafe {
            device
                .CreateVertexShader(blob_bytes(&vs), None, Some(&mut vertex))
                .ctx("CreateVertexShader")?;
            device
                .CreatePixelShader(blob_bytes(&ps_luma), None, Some(&mut luma))
                .ctx("CreatePixelShader(luma)")?;
            device
                .CreatePixelShader(blob_bytes(&ps_chroma), None, Some(&mut chroma))
                .ctx("CreatePixelShader(chroma)")?;
        }

        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            MipLODBias: 0.0,
            MaxAnisotropy: 1,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            BorderColor: [0.0; 4],
            MinLOD: 0.0,
            MaxLOD: 0.0,
        };
        let mut sampler = None;
        // SAFETY: valid descriptor; out-pointer valid for writes.
        unsafe { device.CreateSamplerState(&sampler_desc, Some(&mut sampler)) }
            .ctx("CreateSamplerState")?;

        let buffer_desc = D3D11_BUFFER_DESC {
            ByteWidth: size_of::<Params>() as u32,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        let mut constants = None;
        // SAFETY: valid descriptor; no initial data; out-pointer valid.
        unsafe { device.CreateBuffer(&buffer_desc, None, Some(&mut constants)) }
            .ctx("CreateBuffer(constants)")?;

        let mut planes = Vec::with_capacity(pool.len());
        for slot in 0..pool.len() {
            planes.push(Planes {
                luma: plane_view(device, pool.texture(slot), luma_format)?,
                chroma: plane_view(device, pool.texture(slot), chroma_format)?,
            });
        }

        // SAFETY: plain getter on a valid device.
        let context = unsafe { device.GetImmediateContext() }.ctx("GetImmediateContext")?;
        let multithread: ID3D11Multithread = context.cast().ctx("ID3D11Multithread")?;
        let missing = || VideoError::Other("D3D11 returned a null object".into());
        Ok(Self {
            device: device.clone(),
            context,
            multithread,
            vertex: vertex.ok_or_else(missing)?,
            luma: luma.ok_or_else(missing)?,
            chroma: chroma.ok_or_else(missing)?,
            sampler: sampler.ok_or_else(missing)?,
            constants: constants.ok_or_else(missing)?,
            planes,
            input: None,
            params: None,
            width: pool.width,
            height: pool.height,
            picture,
            scale,
            peak,
        })
    }

    /// Converts `latest` into `slot`'s surface.
    pub(crate) fn blit(
        &mut self,
        latest: &Latest,
        _pool: &Pool,
        slot: usize,
    ) -> Result<(), VideoError> {
        let Some(texture) = latest.texture.as_ref() else {
            return Ok(());
        };
        if self.input.as_ref().is_none_or(|i| i.texture != *texture) {
            self.input = Some(Input {
                texture: texture.clone(),
                view: self.input_view(texture)?,
            });
        }
        let params = self.params_for(latest);
        let planes = &self.planes[slot];
        let input = self.input.as_ref().map(|i| i.view.clone());
        let ctx = &self.context;
        // Other threads use the immediate context too (the capture callback's
        // copies, the encoder): hold its lock across the state we set, so
        // nothing interleaves between setting it and drawing.
        // SAFETY: Enter/Leave are balanced below.
        unsafe { self.multithread.Enter() };
        // SAFETY: every object is on this device and alive for the calls;
        // slices and structs are locals that outlive them.
        unsafe {
            if self.params != Some(params) {
                ctx.UpdateSubresource(&self.constants, 0, None, (&raw const params).cast(), 0, 0);
                self.params = Some(params);
            }
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.IASetInputLayout(None);
            ctx.VSSetShader(&self.vertex, None);
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.PSSetShaderResources(0, Some(&[input]));
            ctx.RSSetState(None);
            ctx.OMSetBlendState(None, None, u32::MAX);
            ctx.OMSetDepthStencilState(None, 0);

            ctx.OMSetRenderTargets(Some(&[Some(planes.luma.clone())]), None);
            ctx.RSSetViewports(Some(&[viewport(self.width, self.height)]));
            ctx.PSSetShader(&self.luma, None);
            ctx.Draw(3, 0);

            ctx.OMSetRenderTargets(Some(&[Some(planes.chroma.clone())]), None);
            ctx.RSSetViewports(Some(&[viewport(self.width / 2, self.height / 2)]));
            ctx.PSSetShader(&self.chroma, None);
            ctx.Draw(3, 0);

            // Unbind, so neither surface stays referenced by the pipeline
            // while the encoder or the capture callback use it.
            ctx.OMSetRenderTargets(None, None);
            ctx.PSSetShaderResources(0, Some(&[None]));
        }
        // SAFETY: balances the Enter above.
        unsafe { self.multithread.Leave() };
        Ok(())
    }

    fn params_for(&self, latest: &Latest) -> Params {
        let (cw, ch) = (latest.content_width, latest.content_height);
        let r = place(cw, ch, self.picture, (self.width, self.height));
        Params {
            dst_origin: [r.left as f32, r.top as f32],
            dst_size: [(r.right - r.left) as f32, (r.bottom - r.top) as f32],
            src_size: [cw as f32, ch as f32],
            tex_size_inv: [
                1.0 / latest.tex_width.max(1) as f32,
                1.0 / latest.tex_height.max(1) as f32,
            ],
            scale: self.scale,
            peak: self.peak,
            _pad: [0.0; 2],
        }
    }

    fn input_view(
        &self,
        texture: &ID3D11Texture2D,
    ) -> Result<ID3D11ShaderResourceView, VideoError> {
        let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                },
            },
        };
        let mut view = None;
        // SAFETY: an FP16 texture with BIND_SHADER_RESOURCE on this device.
        unsafe {
            self.device
                .CreateShaderResourceView(texture, Some(&desc), Some(&mut view))
        }
        .ctx("CreateShaderResourceView(capture)")?;
        view.ok_or_else(|| VideoError::Other("CreateShaderResourceView returned null".into()))
    }
}

/// The single-channel and two-channel view formats that address the luma
/// and chroma planes of a planar surface.
fn plane_formats(format: DXGI_FORMAT) -> Result<(DXGI_FORMAT, DXGI_FORMAT), VideoError> {
    match format {
        DXGI_FORMAT_NV12 => Ok((DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM)),
        DXGI_FORMAT_P010 => Ok((DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16G16_UNORM)),
        other => Err(VideoError::Other(format!(
            "no plane views for surface format {}",
            other.0
        ))),
    }
}

fn check_render_target(device: &ID3D11Device, format: DXGI_FORMAT) -> Result<(), VideoError> {
    // SAFETY: plain capability query.
    let support = unsafe { device.CheckFormatSupport(format) }.unwrap_or(0);
    if support & D3D11_FORMAT_SUPPORT_RENDER_TARGET.0 as u32 == 0 {
        return Err(VideoError::Other(format!(
            "the GPU cannot render to {} surfaces",
            if format == DXGI_FORMAT_P010 {
                "P010"
            } else {
                "NV12"
            }
        )));
    }
    Ok(())
}

fn plane_view(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    format: DXGI_FORMAT,
) -> Result<ID3D11RenderTargetView, VideoError> {
    // For planar formats the view format picks the plane.
    let desc = D3D11_RENDER_TARGET_VIEW_DESC {
        Format: format,
        ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 },
        },
    };
    let mut view = None;
    // SAFETY: a planar render-target texture on this device; out-pointer
    // valid for writes.
    unsafe { device.CreateRenderTargetView(texture, Some(&desc), Some(&mut view)) }
        .ctx("CreateRenderTargetView(plane)")?;
    view.ok_or_else(|| VideoError::Other("CreateRenderTargetView returned null".into()))
}

fn viewport(width: u32, height: u32) -> D3D11_VIEWPORT {
    D3D11_VIEWPORT {
        TopLeftX: 0.0,
        TopLeftY: 0.0,
        Width: width as f32,
        Height: height as f32,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    }
}

fn compile(entry: &str, target: &str, defines: &[(&str, &str)]) -> Result<ID3DBlob, VideoError> {
    let cstr =
        |s: &str| CString::new(s).map_err(|_| VideoError::Other("NUL in shader name".into()));
    let entry = cstr(entry)?;
    let target = cstr(target)?;
    let owned: Vec<(CString, CString)> = defines
        .iter()
        .map(|(n, v)| Ok((cstr(n)?, cstr(v)?)))
        .collect::<Result<_, VideoError>>()?;
    let mut macros: Vec<D3D_SHADER_MACRO> = owned
        .iter()
        .map(|(n, v)| D3D_SHADER_MACRO {
            Name: PCSTR(n.as_ptr().cast()),
            Definition: PCSTR(v.as_ptr().cast()),
        })
        .collect();
    macros.push(D3D_SHADER_MACRO::default());
    let mut code = None;
    let mut errors = None;
    // SAFETY: the source, names and macro strings outlive the call; the
    // macro list is NUL-terminated; out-pointers are valid for writes.
    let result = unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            PCSTR(c"convert.hlsl".as_ptr().cast()),
            Some(macros.as_ptr()),
            None,
            PCSTR(entry.as_ptr().cast()),
            PCSTR(target.as_ptr().cast()),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = result {
        let log = errors.as_ref().map(blob_text).unwrap_or_default();
        return Err(VideoError::Other(format!(
            "shader {} failed to compile: {} {log}",
            entry.to_string_lossy(),
            e.message()
        )));
    }
    code.ok_or_else(|| VideoError::Other("D3DCompile returned no code".into()))
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: the blob owns `GetBufferSize` bytes at `GetBufferPointer` for
    // as long as it lives, and the slice borrows the blob.
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    }
}

fn blob_text(blob: &ID3DBlob) -> String {
    String::from_utf8_lossy(blob_bytes(blob))
        .trim_end_matches('\0')
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    //! The shader maths checked against a CPU reference on the GPU itself:
    //! uniform FP16 blocks in, the Y'CbCr codes read back at block centres.
    //! Needs a D3D11 GPU, so ignored by default:
    //! `cargo test --release -p rec-video shader -- --ignored`.

    use super::*;
    use crate::capture::Latest;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ,
        D3D11_MAPPED_SUBRESOURCE, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

    const BLOCK: u32 = 16;

    /// IEEE half from f32, round to nearest (normal values and zero only).
    fn half(v: f32) -> u16 {
        let bits = v.to_bits();
        let sign = ((bits >> 16) & 0x8000) as u16;
        let abs = v.abs();
        if abs < 6.1e-5 {
            return sign;
        }
        let exp = ((bits >> 23) & 0xFF) as i32 - 127 + 15;
        let mant = bits & 0x7F_FFFF;
        let mut h = ((exp as u32) << 10) | (mant >> 13);
        if mant & 0x1000 != 0 {
            h += 1;
        }
        sign | h as u16
    }

    struct Reference {
        hdr10: bool,
        scale: f64,
        peak: f64,
    }

    impl Reference {
        fn encode(&self, rgb: [f64; 3]) -> [f64; 3] {
            if self.hdr10 {
                let m = [
                    [0.627403896, 0.329283039, 0.043313065],
                    [0.069097289, 0.919540395, 0.011362316],
                    [0.016391439, 0.088013308, 0.895595253],
                ];
                let mut out = [0.0; 3];
                for (o, row) in out.iter_mut().zip(m) {
                    let nits = (row[0] * rgb[0] + row[1] * rgb[1] + row[2] * rgb[2]) * self.scale;
                    let l = (nits.clamp(0.0, self.peak) / 10_000.0).powf(0.1593017578125);
                    *o = ((0.8359375 + 18.8515625 * l) / (1.0 + 18.6875 * l)).powf(78.84375);
                }
                out
            } else {
                let mut x = rgb.map(|c| (c * self.scale).max(0.0));
                let m = x[0].max(x[1]).max(x[2]);
                let knee = 0.85;
                if m > knee {
                    let t = knee + (1.0 - knee) * (1.0 - ((knee - m) / (1.0 - knee)).exp());
                    x = x.map(|c| c * t / m);
                }
                x.map(|c| {
                    let c = c.min(1.0);
                    if c <= 0.0031308 {
                        12.92 * c
                    } else {
                        1.055 * c.powf(1.0 / 2.4) - 0.055
                    }
                })
            }
        }

        /// (Y, Cb, Cr) codes: 10-bit for HDR10, 8-bit otherwise.
        fn codes(&self, rgb: [f64; 3]) -> [f64; 3] {
            let e = self.encode(rgb);
            let (k, cb_div, cr_div, yb, yr, cm, cr) = if self.hdr10 {
                (
                    [0.2627, 0.6780, 0.0593],
                    1.8814,
                    1.4746,
                    64.0,
                    876.0,
                    512.0,
                    896.0,
                )
            } else {
                (
                    [0.2126, 0.7152, 0.0722],
                    1.8556,
                    1.5748,
                    16.0,
                    219.0,
                    128.0,
                    224.0,
                )
            };
            let y = k[0] * e[0] + k[1] * e[1] + k[2] * e[2];
            [
                yb + yr * y,
                cm + cr * (e[2] - y) / cb_div,
                cm + cr * (e[0] - y) / cr_div,
            ]
        }
    }

    /// Converts a `cols` x `rows` grid of uniform blocks and returns the
    /// codes at each block's centre, row by row.
    fn run(colours: &[[f32; 3]], cols: u32, conversion: Conversion) -> Vec<[u32; 3]> {
        let rows = (colours.len() as u32).div_ceil(cols);
        let (w, h) = (cols * BLOCK, rows * BLOCK);
        let gpu = crate::d3d::create_for_source(crate::CaptureSource::PrimaryMonitor).unwrap();
        let device = &gpu.device;
        let mut pixels = vec![0u16; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y / BLOCK) * cols + x / BLOCK) as usize;
                let c = colours.get(i).copied().unwrap_or([0.0; 3]);
                let p = ((y * w + x) * 4) as usize;
                pixels[p..p + 4].copy_from_slice(&[half(c[0]), half(c[1]), half(c[2]), half(1.0)]);
            }
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr().cast(),
            SysMemPitch: w * 8,
            SysMemSlicePitch: 0,
        };
        let mut texture = None;
        // SAFETY: `init` points at w*h*8 bytes that outlive the call.
        unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut texture)) }.unwrap();
        let latest = Latest {
            texture,
            tex_width: w,
            tex_height: h,
            content_width: w,
            content_height: h,
            seq: 1,
        };
        let pool = Pool::new(device, w, h, conversion.output_format()).unwrap();
        let mut converter = ShaderConverter::new(device, &pool, (w, h), conversion).unwrap();
        converter.blit(&latest, &pool, 0).unwrap();

        let staging_desc = D3D11_TEXTURE2D_DESC {
            Format: conversion.output_format(),
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            ..desc
        };
        let mut staging = None;
        // SAFETY: valid descriptor; out-pointer valid.
        unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }.unwrap();
        let staging = staging.unwrap();
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: same size and format; Map waits for the copy.
        unsafe {
            gpu.context.CopyResource(&staging, pool.texture(0));
            gpu.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .unwrap();
        }
        let bytes = if conversion.output_format() == DXGI_FORMAT_P010 {
            2
        } else {
            1
        };
        let pitch = mapped.RowPitch as usize;
        // SAFETY: while mapped, both planes (h + h/2 rows) are readable.
        let data = unsafe {
            std::slice::from_raw_parts(mapped.pData as *const u8, pitch * (h as usize * 3 / 2))
        };
        let sample = |offset: usize| -> u32 {
            if bytes == 2 {
                u32::from(u16::from_le_bytes([data[offset], data[offset + 1]])) >> 6
            } else {
                u32::from(data[offset])
            }
        };
        let mut out = Vec::new();
        for i in 0..colours.len() as u32 {
            let (bx, by) = (
                (i % cols) * BLOCK + BLOCK / 2,
                (i / cols) * BLOCK + BLOCK / 2,
            );
            let y = sample(by as usize * pitch + bx as usize * bytes);
            let uv_row = (h as usize + by as usize / 2) * pitch;
            let uv = uv_row + (bx as usize / 2) * 2 * bytes;
            out.push([y, sample(uv), sample(uv + bytes)]);
        }
        // SAFETY: mapped above.
        unsafe { gpu.context.Unmap(&staging, 0) };
        out
    }

    fn check(colours: &[[f32; 3]], conversion: Conversion, reference: &Reference) {
        let got = run(colours, 4, conversion);
        for (c, g) in colours.iter().zip(&got) {
            let want = reference.codes(c.map(f64::from));
            for k in 0..3 {
                assert!(
                    (f64::from(g[k]) - want[k]).abs() <= 1.0,
                    "{c:?}: got {g:?}, want {want:.1?}"
                );
            }
        }
    }

    const COLOURS: [[f32; 3]; 10] = [
        [0.0, 0.0, 0.0],
        [1.0, 1.0, 1.0],          // SDR white at 80 cd/m²
        [2.5375, 2.5375, 2.5375], // 203 cd/m² reference white
        [1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, 0.0, 1.0],
        [10.0, 10.0, 10.0], // 800 cd/m² highlight
        [4.0, 1.0, 0.5],
        [-0.1, 0.5, 1.2], // outside BT.709
        [0.18, 0.18, 0.18],
    ];

    #[test]
    #[ignore = "needs a D3D11 GPU"]
    fn hdr10_matches_the_reference() {
        let peak = 603.7;
        check(
            &COLOURS,
            Conversion::Hdr10 { peak_nits: peak },
            &Reference {
                hdr10: true,
                scale: 80.0,
                peak: f64::from(peak),
            },
        );
    }

    #[test]
    #[ignore = "needs a D3D11 GPU"]
    fn tonemap_matches_the_reference() {
        let white = 240.0;
        check(
            &COLOURS,
            Conversion::Tonemap {
                sdr_white_nits: white,
            },
            &Reference {
                hdr10: false,
                scale: 80.0 / f64::from(white),
                peak: 1.0,
            },
        );
    }

    #[test]
    #[ignore = "needs a D3D11 GPU"]
    fn sdr_white_stays_white_and_black_stays_black() {
        // With SDR white at 80 cd/m² an SDR desktop maps 1:1 below the knee.
        let got = run(
            &[[0.0; 3], [0.5, 0.5, 0.5], [1.0; 3]],
            3,
            Conversion::Tonemap {
                sdr_white_nits: 80.0,
            },
        );
        assert_eq!(got[0], [16, 128, 128]);
        assert_eq!(got[1][1..], [128, 128]);
        // Above the knee: rolled off a little, never clipped to 235.
        assert!((225..235).contains(&got[2][0]), "{:?}", got[2]);
    }
}
