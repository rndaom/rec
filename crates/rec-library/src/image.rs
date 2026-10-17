//! Stills and contact sheets: WIC scales and writes JPEG, Direct2D and
//! DirectWrite draw the sheets' tiles and labels into a WIC bitmap.

use std::path::Path;

use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, D2D1_DRAW_TEXT_OPTIONS_CLIP,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_RENDER_TARGET_USAGE_NONE, D2D1CreateFactory,
    ID2D1Factory, ID2D1RenderTarget,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatJpeg, GUID_WICPixelFormat24bppBGR,
    GUID_WICPixelFormat32bppBGR, GUID_WICPixelFormat32bppPBGRA, IWICBitmapSource,
    IWICImagingFactory, WICBitmapCacheOnDemand, WICBitmapDitherTypeNone, WICBitmapEncoderNoCache,
    WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom,
};
use windows::Win32::System::Com::StructuredStorage::{IPropertyBag2, PROPBAG2};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, STATFLAG_NONAME, STATSTG, STREAM_SEEK_SET,
};
use windows::Win32::System::Variant::{VARIANT, VT_R4};
use windows::Win32::UI::Shell::SHCreateMemStream;
use windows::core::{GUID, Interface, PWSTR, w};

use crate::decode::Frame;

/// JPEG quality for stills and sheets (0 to 1).
pub const QUALITY: f32 = 0.85;

/// Contact sheet layout.
#[derive(Clone, Copy, Debug)]
pub struct SheetLayout {
    pub columns: u32,
    pub tile_width: u32,
}

impl Default for SheetLayout {
    /// 4 × 384 px tiles: 1556 px wide, about the largest image vision
    /// models take without scaling it down.
    fn default() -> Self {
        Self {
            columns: 4,
            tile_width: 384,
        }
    }
}

const GAP: u32 = 4;
const LABEL: u32 = 22;
const HEADER: u32 = 30;

#[derive(Debug)]
pub struct Imaging {
    wic: IWICImagingFactory,
    d2d: ID2D1Factory,
    label: IDWriteTextFormat,
    header: IDWriteTextFormat,
}

impl Imaging {
    /// Needs COM on the thread ([`crate::sys::init`]).
    pub fn new() -> Result<Self, String> {
        // SAFETY: factory creation with no inputs beyond constants.
        unsafe {
            let wic: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                    .map_err(ctx("WIC"))?;
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)
                .map_err(ctx("D2D1CreateFactory"))?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)
                .map_err(ctx("DWriteCreateFactory"))?;
            let format = |size: f32| -> Result<IDWriteTextFormat, String> {
                let f = dwrite
                    .CreateTextFormat(
                        w!("Segoe UI"),
                        None,
                        DWRITE_FONT_WEIGHT_SEMI_BOLD,
                        DWRITE_FONT_STYLE_NORMAL,
                        DWRITE_FONT_STRETCH_NORMAL,
                        size,
                        w!("en-us"),
                    )
                    .map_err(ctx("CreateTextFormat"))?;
                let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
                Ok(f)
            };
            Ok(Self {
                wic,
                d2d,
                label: format(14.0)?,
                header: format(16.0)?,
            })
        }
    }

    /// `frame` scaled down to `width` (never up), keeping its aspect.
    pub fn scaled(&self, frame: &Frame, width: u32) -> Result<IWICBitmapSource, String> {
        // SAFETY: WIC copies `frame.bgra` (stride `width * 4`, the size
        // given) into its own bitmap.
        unsafe {
            let bitmap = self
                .wic
                .CreateBitmapFromMemory(
                    frame.width,
                    frame.height,
                    &GUID_WICPixelFormat32bppBGR,
                    frame.width * 4,
                    &frame.bgra,
                )
                .map_err(ctx("CreateBitmapFromMemory"))?;
            if width >= frame.width {
                return bitmap.cast().map_err(ctx("IWICBitmapSource"));
            }
            let height = scaled_height(frame.width, frame.height, width);
            let scaler = self
                .wic
                .CreateBitmapScaler()
                .map_err(ctx("CreateBitmapScaler"))?;
            scaler
                .Initialize(&bitmap, width, height, WICBitmapInterpolationModeFant)
                .map_err(ctx("scale"))?;
            scaler.cast().map_err(ctx("IWICBitmapSource"))
        }
    }

    /// A scaled copy of `frame` that owns its pixels (a scaled source only
    /// refers to the original).
    pub fn shrink(&self, frame: &Frame, width: u32) -> Result<Frame, String> {
        let source = self.scaled(frame, width)?;
        let (mut w, mut h) = (0, 0);
        // SAFETY: the buffer holds exactly `h` rows of `w * 4` bytes.
        unsafe {
            source.GetSize(&mut w, &mut h).map_err(ctx("GetSize"))?;
            let mut bgra = vec![0u8; (w * h * 4) as usize];
            source
                .CopyPixels(std::ptr::null(), w * 4, &mut bgra)
                .map_err(ctx("CopyPixels"))?;
            Ok(Frame {
                t: frame.t,
                width: w,
                height: h,
                bgra,
            })
        }
    }

    /// Encodes as JPEG.
    pub fn jpeg(&self, source: &IWICBitmapSource) -> Result<Vec<u8>, String> {
        // SAFETY: WIC encoding into a memory stream this function owns; the
        // property bag entry names a static string.
        unsafe {
            let bgr = self.convert(source, &GUID_WICPixelFormat24bppBGR)?;
            let (mut width, mut height) = (0, 0);
            bgr.GetSize(&mut width, &mut height)
                .map_err(ctx("GetSize"))?;
            let stream = SHCreateMemStream(None).ok_or("SHCreateMemStream failed")?;
            let encoder = self
                .wic
                .CreateEncoder(&GUID_ContainerFormatJpeg, std::ptr::null())
                .map_err(ctx("JPEG encoder"))?;
            encoder
                .Initialize(&stream, WICBitmapEncoderNoCache)
                .map_err(ctx("JPEG encoder"))?;
            let mut frame = None;
            let mut options: Option<IPropertyBag2> = None;
            encoder
                .CreateNewFrame(&mut frame, &mut options)
                .map_err(ctx("CreateNewFrame"))?;
            let frame = frame.ok_or("CreateNewFrame returned no frame")?;
            if let Some(options) = &options {
                let name = PROPBAG2 {
                    vt: VT_R4,
                    pstrName: PWSTR(w!("ImageQuality").as_ptr().cast_mut()),
                    ..PROPBAG2::default()
                };
                let _ = options.Write(1, &name, &VARIANT::from(QUALITY));
            }
            frame
                .Initialize(options.as_ref())
                .map_err(ctx("JPEG frame"))?;
            frame.SetSize(width, height).map_err(ctx("JPEG frame"))?;
            let mut format: GUID = GUID_WICPixelFormat24bppBGR;
            frame
                .SetPixelFormat(&mut format)
                .map_err(ctx("JPEG frame"))?;
            frame
                .WriteSource(&bgr, std::ptr::null())
                .map_err(ctx("JPEG write"))?;
            frame.Commit().map_err(ctx("JPEG write"))?;
            encoder.Commit().map_err(ctx("JPEG write"))?;
            let mut stat = STATSTG::default();
            stream
                .Stat(&mut stat, STATFLAG_NONAME)
                .map_err(ctx("Stat"))?;
            let mut bytes = vec![0u8; stat.cbSize as usize];
            stream.Seek(0, STREAM_SEEK_SET, None).map_err(ctx("Seek"))?;
            let mut read = 0u32;
            stream
                .Read(
                    bytes.as_mut_ptr().cast(),
                    bytes.len() as u32,
                    Some(&mut read),
                )
                .ok()
                .map_err(ctx("Read"))?;
            bytes.truncate(read as usize);
            Ok(bytes)
        }
    }

    pub fn save_jpeg(&self, source: &IWICBitmapSource, path: &Path) -> Result<(), String> {
        let bytes = self.jpeg(source)?;
        std::fs::write(path, bytes).map_err(|e| format!("can't write {}: {e}", path.display()))
    }

    /// A contact sheet: a title line, then the frames in a grid, each with
    /// its label underneath (never over the picture, which may hold the
    /// HUD).
    pub fn sheet(
        &self,
        title: &str,
        tiles: &[(&Frame, String)],
        layout: SheetLayout,
    ) -> Result<IWICBitmapSource, String> {
        let first = tiles.first().ok_or("a contact sheet needs a frame")?.0;
        let tile_w = layout.tile_width;
        let tile_h = scaled_height(first.width, first.height, tile_w);
        let columns = layout.columns.max(1).min(tiles.len() as u32);
        let rows = (tiles.len() as u32).div_ceil(columns);
        let width = columns * tile_w + (columns + 1) * GAP;
        let height = HEADER + rows * (tile_h + LABEL + GAP) + GAP;
        // SAFETY: drawing into a WIC bitmap this function creates; every
        // rectangle is inside it and every string outlives its call.
        unsafe {
            let canvas = self
                .wic
                .CreateBitmap(
                    width,
                    height,
                    &GUID_WICPixelFormat32bppPBGRA,
                    WICBitmapCacheOnDemand,
                )
                .map_err(ctx("CreateBitmap"))?;
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let target: ID2D1RenderTarget = self
                .d2d
                .CreateWicBitmapRenderTarget(&canvas, &props)
                .map_err(ctx("CreateWicBitmapRenderTarget"))?;
            let white = target
                .CreateSolidColorBrush(&rgb(0.95, 0.95, 0.95), None)
                .map_err(ctx("brush"))?;
            let grey = target
                .CreateSolidColorBrush(&rgb(0.7, 0.72, 0.75), None)
                .map_err(ctx("brush"))?;
            target.BeginDraw();
            target.Clear(Some(&rgb(0.09, 0.09, 0.1)));
            let text = |s: &str, format: &IDWriteTextFormat, rect: D2D_RECT_F, brush| {
                let wide: Vec<u16> = s.encode_utf16().collect();
                target.DrawText(
                    &wide,
                    format,
                    &rect,
                    brush,
                    D2D1_DRAW_TEXT_OPTIONS_CLIP,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            };
            text(
                title,
                &self.header,
                rect(GAP + 4, 0, width - GAP, HEADER),
                &white,
            );
            for (i, (frame, label)) in tiles.iter().enumerate() {
                let (col, row) = (i as u32 % columns, i as u32 / columns);
                let x = GAP + col * (tile_w + GAP);
                let y = HEADER + row * (tile_h + LABEL + GAP);
                let scaled = self.scaled(frame, tile_w)?;
                let pbgra = self.convert(&scaled, &GUID_WICPixelFormat32bppPBGRA)?;
                let bitmap = target
                    .CreateBitmapFromWicBitmap(&pbgra, None)
                    .map_err(ctx("CreateBitmapFromWicBitmap"))?;
                // Frames of another size (rare) are fitted to the tile.
                target.DrawBitmap(
                    &bitmap,
                    Some(&rect(x, y, x + tile_w, y + tile_h)),
                    1.0,
                    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                    None,
                );
                text(
                    label,
                    &self.label,
                    rect(x + 3, y + tile_h, x + tile_w, y + tile_h + LABEL),
                    &grey,
                );
            }
            target.EndDraw(None, None).map_err(ctx("EndDraw"))?;
            canvas.cast().map_err(ctx("IWICBitmapSource"))
        }
    }

    fn convert(
        &self,
        source: &IWICBitmapSource,
        format: &GUID,
    ) -> Result<IWICBitmapSource, String> {
        // SAFETY: a WIC converter over a valid source.
        unsafe {
            let converter = self
                .wic
                .CreateFormatConverter()
                .map_err(ctx("CreateFormatConverter"))?;
            converter
                .Initialize(
                    source,
                    format,
                    WICBitmapDitherTypeNone,
                    None,
                    0.0,
                    WICBitmapPaletteTypeCustom,
                )
                .map_err(ctx("convert"))?;
            converter.cast().map_err(ctx("IWICBitmapSource"))
        }
    }
}

/// Height of a `width × height` picture scaled to `to` wide, at least 2 px
/// and even.
pub fn scaled_height(width: u32, height: u32, to: u32) -> u32 {
    let h = (u64::from(height) * u64::from(to) + u64::from(width) / 2) / u64::from(width.max(1));
    (h as u32).max(2) & !1
}

fn rect(left: u32, top: u32, right: u32, bottom: u32) -> D2D_RECT_F {
    D2D_RECT_F {
        left: left as f32,
        top: top as f32,
        right: right as f32,
        bottom: bottom as f32,
    }
}

fn rgb(r: f32, g: f32, b: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a: 1.0 }
}

fn ctx(what: &'static str) -> impl Fn(windows::core::Error) -> String {
    move |e| format!("{what}: {e}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32, t: f64) -> Frame {
        let mut bgra = vec![0u8; (width * height * 4) as usize];
        for (i, px) in bgra.chunks_mut(4).enumerate() {
            let x = i as u32 % width;
            px.copy_from_slice(&[(x * 255 / width) as u8, 80, 200, 0]);
        }
        Frame {
            t,
            width,
            height,
            bgra,
        }
    }

    #[test]
    fn sizes() {
        assert_eq!(scaled_height(1920, 1080, 1024), 576);
        assert_eq!(scaled_height(2560, 1440, 384), 216);
        assert_eq!(scaled_height(1128, 640, 384), 218);
        assert_eq!(scaled_height(1, 1, 1), 2);
    }

    #[test]
    fn writes_jpeg_stills_and_sheets() {
        crate::sys::init().unwrap();
        let imaging = Imaging::new().unwrap();
        let f = frame(640, 360, 1.0);
        let still = imaging.scaled(&f, 320).unwrap();
        let jpeg = imaging.jpeg(&still).unwrap();
        assert_eq!(&jpeg[..3], &[0xFF, 0xD8, 0xFF], "JPEG SOI marker");
        assert_eq!(&jpeg[jpeg.len() - 2..], &[0xFF, 0xD9], "JPEG EOI marker");
        let mut size = (0, 0);
        // SAFETY: plain query.
        unsafe { still.GetSize(&mut size.0, &mut size.1) }.unwrap();
        assert_eq!(size, (320, 180));

        let frames: Vec<Frame> = (0..6).map(|i| frame(640, 360, f64::from(i))).collect();
        let tiles: Vec<(&Frame, String)> = frames
            .iter()
            .map(|f| (f, format!("0:0{}.0 · every 1 s", f.t)))
            .collect();
        let sheet = imaging
            .sheet("clip.mp4 · sheet 1 of 1", &tiles, SheetLayout::default())
            .unwrap();
        // SAFETY: plain query.
        unsafe { sheet.GetSize(&mut size.0, &mut size.1) }.unwrap();
        assert_eq!(
            size,
            (4 * 384 + 5 * GAP, HEADER + 2 * (216 + LABEL + GAP) + GAP)
        );
        assert!(imaging.jpeg(&sheet).unwrap().len() > 1000);
    }
}
