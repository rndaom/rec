//! Native AMD AMF encoder backend (H.264, HEVC, AV1), loaded at runtime from
//! the driver's `amfrt64.dll` (decision D4: driver encoders, no bundled
//! dependencies).
//!
//! Compared with the AMD Media Foundation MFTs this gives AV1, explicit
//! 10-bit (HEVC Main10 / AV1 10-bit) with the colour description written
//! into the bitstream, and direct control of rate control and GOP structure.
//! Bindings are hand-written for the small subset used (see [`sys`]); the
//! runtime is optional, so every failure (DLL absent or too old, codec or
//! 10-bit unsupported, init errors) is an `Err` the caller can fall back on.
//!
//! # Input textures
//!
//! The caller's NV12 / P010 textures are wrapped without a copy
//! (`CreateSurfaceFromDX11Native`) with an `AMFSurfaceObserver` attached.
//! The runtime calls the observer when the last reference to the wrapping
//! surface goes away, i.e. when the encoder no longer reads the texture;
//! that is the moment a tag is reported back through
//! [`AmfEncoder::poll`]'s `on_release`. Until then we also keep our own
//! reference to the texture, so it outlives the surface whatever the runtime
//! does with it. Measured on an RX 7800 XT (AMF 1.5.2): every tag comes back
//! exactly once, no later than the poll that returns its output (one texture
//! in flight at 60 fps), so no copy into runtime-allocated surfaces is
//! needed. The input queue holds 16 frames before `AMF_INPUT_FULL`.
//!
//! # Stream contract
//!
//! Low-latency usage, no B-frames, no pre-analysis or look-ahead, so output
//! order is input order and `pts` passes through unchanged. A keyframe is
//! forced on every `fps`-th accepted frame, with the parameter sets / AV1
//! sequence header forced into it; the encoder's own IDR period and header
//! insertion are set to the same cadence. Rate control is peak-constrained
//! VBR (mean = kbps, peak = 1.5 x mean, VBV = one second at the peak). HDR
//! mastering metadata is deliberately not given to AMF: the output stage
//! inserts it for every backend alike.
//!
//! # Sizes
//!
//! H.264 and HEVC code any even size exactly. The RDNA3 AV1 engine works in
//! 64 x 16 blocks and no alignment mode crops the padding back, so
//! [`AmfEncoder::new`] refuses AV1 sizes that are not multiples of the
//! alignment the encoder reports; [`size_alignment`] tells the caller what to
//! pad (letterbox) to.

mod sys;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use rec_core::media::{VideoCodec, VideoColor};
use rec_core::time::Hns;
use windows::Win32::Graphics::Direct3D11::{D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_NV12, DXGI_FORMAT_P010};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{Interface, PCWSTR, s, w};

use crate::VideoError;
use sys::{
    AmfResult, ComponentVtbl, ContextVtbl, DataVtbl, FactoryVtbl, Object, PropertyStorageVtbl,
    StorageVtbl, SurfaceObserver, SurfaceObserverVtbl, Variant, props,
};

/// Oldest runtime accepted. A conservative floor around the RDNA3 launch
/// drivers (late 2022), which brought the AV1 encoder; only 1.5.2 is tested.
/// Older runtimes are refused rather than half-configured (unknown
/// properties that matter make `new` fail anyway).
const MIN_VERSION: u64 = sys::make_version(1, 4, 28, 0);
/// The SDK header version these bindings follow; passed to `AMFInit`
/// (capped at the runtime's own version) so the runtime knows what we expect.
const HEADER_VERSION: u64 = sys::make_version(1, 5, 2, 0);

pub(crate) struct AmfParams {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub color: VideoColor,
}

// ---------------------------------------------------------------------------
// Runtime loading

/// The loaded runtime: the AMF factory singleton and the runtime version.
struct Runtime {
    factory: NonNull<Object<FactoryVtbl>>,
    version: u64,
}

// SAFETY: the factory is a process-wide singleton owned by the runtime and
// documented as usable from any thread; we never free it.
unsafe impl Send for Runtime {}
// SAFETY: as above; it has no state we mutate.
unsafe impl Sync for Runtime {}

enum Load {
    /// `amfrt64.dll` could not be loaded (no AMD driver).
    Missing,
    /// Loaded but unusable; `version` if it could be queried.
    Failed {
        // Only reported by `runtime_version`; the reason says it too.
        #[cfg_attr(not(test), allow(dead_code))]
        version: Option<u64>,
        reason: String,
    },
    Ready(Runtime),
}

/// Loads and initialises the runtime once per process. The DLL is never
/// unloaded: the factory and any encoder may live until process exit.
fn load() -> &'static Load {
    static RUNTIME: OnceLock<Load> = OnceLock::new();
    RUNTIME.get_or_init(load_runtime)
}

fn load_runtime() -> Load {
    // SAFETY: a plain library load by name, restricted to System32 so no
    // planted copy on the DLL search path is picked up.
    let module =
        match unsafe { LoadLibraryExW(w!("amfrt64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) } {
            Ok(m) => m,
            Err(_) => return Load::Missing,
        };
    let failed = |version, reason: &str| Load::Failed {
        version,
        reason: reason.to_string(),
    };
    // SAFETY: valid module handle and NUL-terminated export name.
    let Some(query) = (unsafe { GetProcAddress(module, s!("AMFQueryVersion")) }) else {
        return failed(None, "amfrt64.dll has no AMFQueryVersion export");
    };
    // SAFETY: the export has the `AMFQueryVersion_Fn` signature (Factory.h).
    let query: sys::QueryVersionFn = unsafe { std::mem::transmute(query) };
    let mut version = 0u64;
    // SAFETY: the out-pointer is valid for writes.
    let r = unsafe { query(&mut version) };
    if r != sys::AMF_OK {
        return failed(None, &format!("AMFQueryVersion failed: {}", result_name(r)));
    }
    if version < MIN_VERSION {
        return failed(
            Some(version),
            &format!(
                "AMF runtime {} is older than the required {}",
                format_version(version),
                format_version(MIN_VERSION)
            ),
        );
    }
    // SAFETY: as above.
    let Some(init) = (unsafe { GetProcAddress(module, s!("AMFInit")) }) else {
        return failed(Some(version), "amfrt64.dll has no AMFInit export");
    };
    // SAFETY: the export has the `AMFInit_Fn` signature (Factory.h).
    let init: sys::InitFn = unsafe { std::mem::transmute(init) };
    let mut factory = std::ptr::null_mut();
    // SAFETY: the out-pointer is valid for writes; the factory returned is a
    // runtime-owned singleton (not reference counted).
    let r = unsafe { init(version.min(HEADER_VERSION), &mut factory) };
    match NonNull::new(factory) {
        Some(factory) if r == sys::AMF_OK => Load::Ready(Runtime { factory, version }),
        _ => failed(
            Some(version),
            &format!("AMFInit failed: {}", result_name(r)),
        ),
    }
}

fn runtime() -> Result<&'static Runtime, VideoError> {
    match load() {
        Load::Ready(rt) => Ok(rt),
        Load::Missing => Err(VideoError::Other(
            "AMD AMF runtime (amfrt64.dll) not found".into(),
        )),
        Load::Failed { reason, .. } => Err(VideoError::Other(reason.clone())),
    }
}

/// The installed AMF runtime version ("major.minor.release"), or `None` when
/// `amfrt64.dll` is absent or does not report one. (Encoder names carry the
/// version for everything else.)
#[cfg(test)]
pub(crate) fn runtime_version() -> Option<String> {
    match load() {
        Load::Ready(rt) => Some(format_version(rt.version)),
        Load::Failed { version, .. } => version.map(format_version),
        Load::Missing => None,
    }
}

fn format_version(v: u64) -> String {
    format!(
        "{}.{}.{}",
        (v >> 48) & 0xFFFF,
        (v >> 32) & 0xFFFF,
        (v >> 16) & 0xFFFF
    )
}

fn result_name(r: AmfResult) -> String {
    usize::try_from(r)
        .ok()
        .and_then(|i| sys::RESULT_NAMES.get(i))
        .map_or_else(|| format!("AMF_RESULT {r}"), |n| (*n).to_string())
}

// ---------------------------------------------------------------------------
// Owned interface pointers

/// An owned reference to an AMF object whose vtable starts with the
/// AMFInterface and AMFPropertyStorage tables; released on drop.
struct Ptr<V: StorageVtbl>(NonNull<Object<V>>);

impl<V: StorageVtbl> Ptr<V> {
    /// # Safety
    /// `p` is null or an owned (+1) reference to a live AMF object whose
    /// vtable has the layout `V`.
    unsafe fn from_raw(p: *mut Object<V>) -> Option<Self> {
        NonNull::new(p).map(Self)
    }

    fn raw(&self) -> *mut c_void {
        self.0.as_ptr().cast()
    }

    fn vtbl(&self) -> &V {
        // SAFETY: the object is alive while we hold a reference, and its
        // vtable is static data of the runtime.
        unsafe { &*self.0.as_ref().vtbl }
    }

    fn set(&self, name: PCWSTR, value: Variant) -> AmfResult {
        // SAFETY: valid object, NUL-terminated name; the variant is a plain
        // value (no owned memory) passed by value as the header declares.
        unsafe { (self.vtbl().storage().set_property)(self.raw(), name.0, value) }
    }

    /// An integer or boolean property, `None` if unset or of another type.
    fn get_i64(&self, name: PCWSTR) -> Option<i64> {
        let mut v = Variant::empty();
        // SAFETY: valid object and name; `v` is a valid out-variant. Only
        // integer/boolean results are read, which own no memory.
        let r = unsafe { (self.vtbl().storage().get_property)(self.raw(), name.0, &mut v) };
        if r == sys::AMF_OK { v.as_i64() } else { None }
    }
}

impl<V: StorageVtbl> Drop for Ptr<V> {
    fn drop(&mut self) {
        // SAFETY: releases the one reference this value owns.
        unsafe { (self.vtbl().storage().base.release)(self.raw()) };
    }
}

fn prop_name(name: PCWSTR) -> String {
    // SAFETY: every name is a NUL-terminated `w!` literal.
    unsafe { name.to_string() }.unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Input release tracking

/// Textures the runtime may still read, keyed by the address of the AMF
/// surface wrapping them, and the tags whose surfaces are gone.
#[derive(Default)]
struct Inflight {
    pending: HashMap<usize, (usize, ID3D11Texture2D)>,
    released: Vec<usize>,
}

/// Our `AMFSurfaceObserver`: the C struct first, then our state. One per
/// encoder, shared by all its surfaces.
#[repr(C)]
struct ReleaseObserver {
    base: SurfaceObserver,
    inflight: Arc<Mutex<Inflight>>,
}

static OBSERVER_VTBL: SurfaceObserverVtbl = SurfaceObserverVtbl {
    on_surface_data_release,
};

fn lock(m: &Mutex<Inflight>) -> MutexGuard<'_, Inflight> {
    // The data stays consistent even if a holder panicked.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Called by the runtime (on any of its threads, or inside our own calls)
/// when a surface we created is destroyed.
unsafe extern "system" fn on_surface_data_release(
    this: *mut SurfaceObserver,
    surface: *mut c_void,
) {
    // SAFETY: `this` is the `base` field at offset 0 of a `ReleaseObserver`
    // that stays allocated while any of its surfaces may exist (see Drop).
    let observer = unsafe { &*this.cast::<ReleaseObserver>() };
    let texture = {
        let mut inflight = lock(&observer.inflight);
        inflight
            .pending
            .remove(&(surface as usize))
            .map(|(tag, texture)| {
                inflight.released.push(tag);
                texture
            })
    };
    // Our texture reference goes outside the lock.
    drop(texture);
}

// ---------------------------------------------------------------------------
// The encoder

/// Codec-specific property names and values.
struct Names {
    usage: PCWSTR,
    usage_low_latency: i64,
    profile: PCWSTR,
    quality_preset: PCWSTR,
    preset_balanced: i64,
    frame_size: PCWSTR,
    frame_rate: PCWSTR,
    rate_control: PCWSTR,
    target_bitrate: PCWSTR,
    peak_bitrate: PCWSTR,
    vbv_buffer_size: PCWSTR,
    skip_frame: PCWSTR,
    query_timeout: PCWSTR,
    bit_depth: PCWSTR,
    out_full_range: PCWSTR,
    in_full_range: PCWSTR,
    out_profile: PCWSTR,
    in_profile: PCWSTR,
    out_transfer: PCWSTR,
    in_transfer: PCWSTR,
    out_primaries: PCWSTR,
    in_primaries: PCWSTR,
    /// Per-frame: force a keyframe (`force_key`) carrying headers.
    force_type: PCWSTR,
    force_key: i64,
    insert_headers: [Option<PCWSTR>; 2],
    /// On output buffers: frame type, `output_key` for a keyframe.
    output_type: PCWSTR,
    output_key: i64,
}

fn names(codec: VideoCodec) -> Names {
    use props::{av1, avc, hevc};
    match codec {
        VideoCodec::H264 => Names {
            usage: avc::USAGE,
            usage_low_latency: avc::USAGE_LOW_LATENCY,
            profile: avc::PROFILE,
            quality_preset: avc::QUALITY_PRESET,
            preset_balanced: avc::QUALITY_PRESET_BALANCED,
            frame_size: avc::FRAMESIZE,
            frame_rate: avc::FRAMERATE,
            rate_control: avc::RATE_CONTROL_METHOD,
            target_bitrate: avc::TARGET_BITRATE,
            peak_bitrate: avc::PEAK_BITRATE,
            vbv_buffer_size: avc::VBV_BUFFER_SIZE,
            skip_frame: avc::RATE_CONTROL_SKIP_FRAME_ENABLE,
            query_timeout: avc::QUERY_TIMEOUT,
            bit_depth: avc::COLOR_BIT_DEPTH,
            out_full_range: avc::OUTPUT_FULL_RANGE_COLOR,
            in_full_range: avc::INPUT_FULL_RANGE_COLOR,
            out_profile: avc::OUTPUT_COLOR_PROFILE,
            in_profile: avc::INPUT_COLOR_PROFILE,
            out_transfer: avc::OUTPUT_TRANSFER_CHARACTERISTIC,
            in_transfer: avc::INPUT_TRANSFER_CHARACTERISTIC,
            out_primaries: avc::OUTPUT_COLOR_PRIMARIES,
            in_primaries: avc::INPUT_COLOR_PRIMARIES,
            force_type: avc::FORCE_PICTURE_TYPE,
            force_key: avc::PICTURE_TYPE_IDR,
            insert_headers: [Some(avc::INSERT_SPS), Some(avc::INSERT_PPS)],
            output_type: avc::OUTPUT_DATA_TYPE,
            output_key: avc::OUTPUT_DATA_TYPE_IDR,
        },
        VideoCodec::Hevc => Names {
            usage: hevc::USAGE,
            usage_low_latency: hevc::USAGE_LOW_LATENCY,
            profile: hevc::PROFILE,
            quality_preset: hevc::QUALITY_PRESET,
            preset_balanced: hevc::QUALITY_PRESET_BALANCED,
            frame_size: hevc::FRAMESIZE,
            frame_rate: hevc::FRAMERATE,
            rate_control: hevc::RATE_CONTROL_METHOD,
            target_bitrate: hevc::TARGET_BITRATE,
            peak_bitrate: hevc::PEAK_BITRATE,
            vbv_buffer_size: hevc::VBV_BUFFER_SIZE,
            skip_frame: hevc::RATE_CONTROL_SKIP_FRAME_ENABLE,
            query_timeout: hevc::QUERY_TIMEOUT,
            bit_depth: hevc::COLOR_BIT_DEPTH,
            out_full_range: hevc::OUTPUT_FULL_RANGE_COLOR,
            in_full_range: hevc::INPUT_FULL_RANGE_COLOR,
            out_profile: hevc::OUTPUT_COLOR_PROFILE,
            in_profile: hevc::INPUT_COLOR_PROFILE,
            out_transfer: hevc::OUTPUT_TRANSFER_CHARACTERISTIC,
            in_transfer: hevc::INPUT_TRANSFER_CHARACTERISTIC,
            out_primaries: hevc::OUTPUT_COLOR_PRIMARIES,
            in_primaries: hevc::INPUT_COLOR_PRIMARIES,
            force_type: hevc::FORCE_PICTURE_TYPE,
            force_key: hevc::PICTURE_TYPE_IDR,
            insert_headers: [Some(hevc::INSERT_HEADER), None],
            output_type: hevc::OUTPUT_DATA_TYPE,
            output_key: hevc::OUTPUT_DATA_TYPE_IDR,
        },
        VideoCodec::Av1 => Names {
            usage: av1::USAGE,
            usage_low_latency: av1::USAGE_LOW_LATENCY,
            profile: av1::PROFILE,
            quality_preset: av1::QUALITY_PRESET,
            preset_balanced: av1::QUALITY_PRESET_BALANCED,
            frame_size: av1::FRAMESIZE,
            frame_rate: av1::FRAMERATE,
            rate_control: av1::RATE_CONTROL_METHOD,
            target_bitrate: av1::TARGET_BITRATE,
            peak_bitrate: av1::PEAK_BITRATE,
            vbv_buffer_size: av1::VBV_BUFFER_SIZE,
            skip_frame: av1::RATE_CONTROL_SKIP_FRAME,
            query_timeout: av1::QUERY_TIMEOUT,
            bit_depth: av1::COLOR_BIT_DEPTH,
            out_full_range: av1::OUTPUT_FULL_RANGE_COLOR,
            in_full_range: av1::INPUT_FULL_RANGE_COLOR,
            out_profile: av1::OUTPUT_COLOR_PROFILE,
            in_profile: av1::INPUT_COLOR_PROFILE,
            out_transfer: av1::OUTPUT_TRANSFER_CHARACTERISTIC,
            in_transfer: av1::INPUT_TRANSFER_CHARACTERISTIC,
            out_primaries: av1::OUTPUT_COLOR_PRIMARIES,
            in_primaries: av1::INPUT_COLOR_PRIMARIES,
            force_type: av1::FORCE_FRAME_TYPE,
            force_key: av1::FORCE_FRAME_TYPE_KEY,
            insert_headers: [Some(av1::FORCE_INSERT_SEQUENCE_HEADER), None],
            output_type: av1::OUTPUT_FRAME_TYPE,
            output_key: av1::OUTPUT_FRAME_TYPE_KEY,
        },
    }
}

/// Peak-constrained VBR rate-control method; 2 in all three encoders.
const RATE_CONTROL_PEAK_CONSTRAINED_VBR: i64 = 2;

/// Mean and peak bit rate in bit/s for a mean of `kbps`: peak = 1.5 x mean,
/// the same rule as the Media Foundation backend (`encoder::bitrate_bps`).
fn bitrate_bps(kbps: u32) -> (u32, u32) {
    let mean = kbps.saturating_mul(1000);
    (mean, mean.saturating_add(mean / 2))
}

fn rate_control_name(codec: VideoCodec, mode: i64) -> &'static str {
    // H.264 numbers its rate-control methods differently from HEVC and AV1.
    let mode = match (codec, mode) {
        (VideoCodec::H264, 1) => 3,
        (VideoCodec::H264, 3) => 1,
        (_, m) => m,
    };
    match mode {
        0 => "constant QP",
        1 => "latency-constrained VBR",
        2 => "peak-constrained VBR",
        3 => "CBR",
        4 => "quality VBR",
        5 => "high-quality VBR",
        6 => "high-quality CBR",
        _ => "unknown",
    }
}

pub(crate) struct AmfEncoder {
    /// Taken (terminated) first in `Drop`.
    session: Option<Session>,
    observer: NonNull<ReleaseObserver>,
    inflight: Arc<Mutex<Inflight>>,
    names: Names,
    codec: VideoCodec,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    /// Keyframe interval in frames (= fps).
    gop: u64,
    /// Frames accepted by the encoder so far.
    frames: u64,
    mean_bps: u32,
    peak_bps: u32,
    name: String,
    rate_control: String,
}

// SAFETY: the AMF context and component may be used from any thread as long
// as calls are not concurrent, which `&mut self` guarantees; the observer
// state is behind a mutex.
unsafe impl Send for AmfEncoder {}

impl std::fmt::Debug for AmfEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AmfEncoder")
            .field("name", &self.name)
            .field("rate_control", &self.rate_control)
            .finish_non_exhaustive()
    }
}

impl AmfEncoder {
    /// Creates and configures an encoder session on `device`. Errs (never
    /// panics) if the runtime is missing or too old, the GPU lacks the codec
    /// or 10-bit, or initialisation fails.
    pub(crate) fn new(device: &ID3D11Device, params: &AmfParams) -> Result<Self, VideoError> {
        validate(params)?;
        let hdr = params.color.is_hdr();
        let session = Session::new(device, params.codec)?;
        let component = &session.component;
        let names = names(params.codec);

        if hdr && params.codec == VideoCodec::Hevc {
            let caps = caps(component)?;
            if caps
                .get_i64(props::hevc::CAP_MAX_PROFILE)
                .is_none_or(|p| p < props::hevc::PROFILE_MAIN_10)
            {
                return Err(VideoError::Other(
                    "the AMD HEVC encoder on this GPU has no Main10 (10-bit) profile".into(),
                ));
            }
        }
        if params.codec == VideoCodec::Av1 {
            let (aw, ah) = av1_alignment(component)?;
            if params.width % aw != 0 || params.height % ah != 0 {
                return Err(VideoError::InvalidConfig(format!(
                    "the AMD AV1 encoder on this GPU cannot code {}x{} exactly: width must be a \
                     multiple of {aw} and height of {ah} (pad to {}x{})",
                    params.width,
                    params.height,
                    params.width.next_multiple_of(aw),
                    params.height.next_multiple_of(ah),
                )));
            }
        }

        let (mean_bps, peak_bps) = bitrate_bps(params.bitrate_kbps);
        configure(component, &names, params, mean_bps, peak_bps)?;

        let (surface_format, format) = if hdr {
            (sys::AMF_SURFACE_P010, DXGI_FORMAT_P010)
        } else {
            (sys::AMF_SURFACE_NV12, DXGI_FORMAT_NV12)
        };
        // SAFETY: valid component; sizes were validated to fit in i32.
        let r = unsafe {
            (component.vtbl().init)(
                component.raw(),
                surface_format,
                params.width as i32,
                params.height as i32,
            )
        };
        if r != sys::AMF_OK {
            return Err(VideoError::Other(format!(
                "AMF {} encoder Init ({}x{} {}) failed: {}",
                params.codec.name(),
                params.width,
                params.height,
                if hdr { "P010" } else { "NV12" },
                result_name(r)
            )));
        }

        let rate_control = describe(component, &names, params.codec);
        let inflight = Arc::new(Mutex::new(Inflight::default()));
        let observer = Box::new(ReleaseObserver {
            base: SurfaceObserver {
                vtbl: &OBSERVER_VTBL,
            },
            inflight: inflight.clone(),
        });
        let version = runtime()?.version;
        Ok(Self {
            session: Some(session),
            observer: NonNull::from(Box::leak(observer)),
            inflight,
            names,
            codec: params.codec,
            width: params.width,
            height: params.height,
            format,
            gop: u64::from(params.fps),
            frames: 0,
            mean_bps,
            peak_bps,
            name: format!(
                "AMD AMF {} (runtime {})",
                params.codec.name(),
                format_version(version)
            ),
            rate_control,
        })
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn codec(&self) -> VideoCodec {
        self.codec
    }

    /// Rate-control description read back right after configuration; not
    /// updated by [`AmfEncoder::set_bitrate`].
    pub(crate) fn rate_control(&self) -> &str {
        &self.rate_control
    }

    /// Size of the coded pictures: always the requested size, since sizes
    /// the hardware would pad (AV1 on RDNA3) are refused by
    /// [`new`](Self::new); see [`size_alignment`].
    pub(crate) fn coded_size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn session(&self) -> &Session {
        // Only `Drop` takes it.
        self.session.as_ref().expect("session present until drop")
    }

    fn component(&self) -> &Ptr<ComponentVtbl> {
        &self.session().component
    }

    fn context(&self) -> &Ptr<ContextVtbl> {
        &self.session().context
    }

    /// Queues one frame. `texture` must be an NV12 (SDR) or P010 (HDR10)
    /// texture of exactly the configured size on the encoder's device; the
    /// encoder may read it until `tag` comes back through
    /// [`poll`](Self::poll)'s `on_release`. `Ok(false)`: the input queue is
    /// full and the frame was not taken (its tag is never reported).
    pub(crate) fn submit(
        &mut self,
        texture: &ID3D11Texture2D,
        tag: usize,
        pts: Hns,
        duration: Hns,
    ) -> Result<bool, VideoError> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: plain getter filling a struct.
        unsafe { texture.GetDesc(&mut desc) };
        if desc.Format != self.format || desc.Width != self.width || desc.Height != self.height {
            return Err(VideoError::InvalidConfig(format!(
                "AMF input texture is {}x{} format {}, expected {}x{} format {}",
                desc.Width, desc.Height, desc.Format.0, self.width, self.height, self.format.0
            )));
        }

        let mut raw = std::ptr::null_mut();
        // SAFETY: valid context; the texture is a live D3D11 texture on the
        // context's device; the observer outlives every surface (see Drop).
        let r = unsafe {
            (self.context().vtbl().create_surface_from_dx11_native)(
                self.context().raw(),
                texture.as_raw(),
                &mut raw,
                self.observer.as_ptr().cast(),
            )
        };
        check(r, "CreateSurfaceFromDX11Native")?;
        // SAFETY: owned reference to an AMFSurface (an AMFData).
        let surface: Ptr<DataVtbl> = unsafe { Ptr::from_raw(raw) }
            .ok_or_else(|| VideoError::Other("AMF returned a null surface".into()))?;
        let key = surface.raw() as usize;
        lock(&self.inflight)
            .pending
            .insert(key, (tag, texture.clone()));

        // SAFETY: plain setters on our surface.
        unsafe {
            (surface.vtbl().set_pts)(surface.raw(), pts);
            (surface.vtbl().set_duration)(surface.raw(), duration);
        }
        let mut result = Ok(());
        if self.frames % self.gop == 0 {
            result = require(
                &surface,
                self.names.force_type,
                Variant::int(self.names.force_key),
            );
            for name in self.names.insert_headers.into_iter().flatten() {
                result = result.and_then(|()| require(&surface, name, Variant::bool(true)));
            }
        }
        let r = match result {
            // SAFETY: valid component and surface; the component takes its
            // own reference if it accepts the surface.
            Ok(()) => unsafe {
                (self.component().vtbl().submit_input)(self.component().raw(), surface.raw())
            },
            Err(e) => {
                self.forget(key, surface);
                return Err(e);
            }
        };
        match r {
            sys::AMF_OK | sys::AMF_NEED_MORE_INPUT => {
                self.frames += 1;
                Ok(true)
            }
            sys::AMF_INPUT_FULL => {
                self.forget(key, surface);
                Ok(false)
            }
            r => {
                self.forget(key, surface);
                Err(amf_err("SubmitInput", r))
            }
        }
    }

    /// Un-registers a surface the encoder did not take, so its tag is not
    /// reported, then drops our reference to it.
    fn forget(&self, key: usize, surface: Ptr<DataVtbl>) {
        let entry = lock(&self.inflight).pending.remove(&key);
        drop(surface);
        drop(entry);
    }

    /// Drains finished output without waiting: every access unit / temporal
    /// unit as `on_output(data, pts, keyframe)`, then every tag whose texture
    /// the encoder no longer reads as `on_release(tag)`.
    pub(crate) fn poll(
        &mut self,
        mut on_output: impl FnMut(&[u8], Hns, bool),
        mut on_release: impl FnMut(usize),
    ) -> Result<(), VideoError> {
        loop {
            let mut raw = std::ptr::null_mut();
            // SAFETY: valid component and out-pointer; the query timeout is
            // 0, so this never blocks.
            let r =
                unsafe { (self.component().vtbl().query_output)(self.component().raw(), &mut raw) };
            // SAFETY: an owned reference when non-null, whatever `r` says.
            let data: Option<Ptr<DataVtbl>> = unsafe { Ptr::from_raw(raw) };
            match (r, data) {
                (sys::AMF_OK, Some(data)) => self.emit(&data, &mut on_output)?,
                (sys::AMF_OK | sys::AMF_REPEAT | sys::AMF_EOF, _) => break,
                (r, _) => return Err(amf_err("QueryOutput", r)),
            }
        }
        let released = std::mem::take(&mut lock(&self.inflight).released);
        for tag in released {
            on_release(tag);
        }
        Ok(())
    }

    fn emit(
        &self,
        data: &Ptr<DataVtbl>,
        on_output: &mut impl FnMut(&[u8], Hns, bool),
    ) -> Result<(), VideoError> {
        // SAFETY: plain calls on a valid AMFData. Encoder output is host
        // memory already; `Convert` is a no-op safeguard otherwise.
        unsafe {
            if (data.vtbl().get_memory_type)(data.raw()) != sys::AMF_MEMORY_HOST {
                check(
                    (data.vtbl().convert)(data.raw(), sys::AMF_MEMORY_HOST),
                    "Convert(host)",
                )?;
            }
        }
        let mut raw = std::ptr::null_mut();
        // SAFETY: valid object, IID and out-pointer; returns an owned
        // reference to the same object as an AMFBuffer.
        let r = unsafe {
            (data.vtbl().storage.base.query_interface)(data.raw(), &sys::IID_AMF_BUFFER, &mut raw)
        };
        check(r, "QueryInterface(AMFBuffer)")?;
        // SAFETY: owned reference to an AMFBuffer.
        let buffer: Ptr<sys::BufferVtbl> = unsafe { Ptr::from_raw(raw.cast()) }
            .ok_or_else(|| VideoError::Other("AMF output is not a buffer".into()))?;
        // SAFETY: plain getters on a valid buffer.
        let (pts, ptr, len) = unsafe {
            (
                (buffer.vtbl().data.get_pts)(buffer.raw()),
                (buffer.vtbl().get_native)(buffer.raw()),
                (buffer.vtbl().get_size)(buffer.raw()),
            )
        };
        let keyframe = buffer.get_i64(self.names.output_type) == Some(self.names.output_key);
        if !ptr.is_null() && len > 0 {
            // SAFETY: host memory of `len` bytes owned by the buffer, which
            // we hold until after the callback.
            let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
            on_output(bytes, pts, keyframe);
        }
        Ok(())
    }

    /// Changes the mean bit rate (peak = 1.5 x mean, VBV = one second at the
    /// peak) of the running session: no restart, the stream stays
    /// continuous. Peak and mean are ordered so peak >= mean at every step.
    /// Returns the mean in kbit/s the encoder reports afterwards.
    pub(crate) fn set_bitrate(&mut self, kbps: u32) -> Result<u32, VideoError> {
        if kbps == 0 {
            return Err(VideoError::InvalidConfig("bitrate must be positive".into()));
        }
        let (mean, peak) = bitrate_bps(kbps);
        if mean > self.mean_bps {
            self.set_peak(peak)?;
            self.set_mean(mean)?;
        } else {
            self.set_mean(mean)?;
            self.set_peak(peak)?;
        }
        Ok(self
            .component()
            .get_i64(self.names.target_bitrate)
            .and_then(|bps| u32::try_from(bps.saturating_add(500) / 1000).ok())
            .unwrap_or(kbps))
    }

    // Each records what is now set, so the ordering in `set_bitrate` stays
    // right even after a partial failure.
    fn set_mean(&mut self, bps: u32) -> Result<(), VideoError> {
        require(
            self.component(),
            self.names.target_bitrate,
            Variant::int(bps.into()),
        )?;
        self.mean_bps = bps;
        Ok(())
    }

    fn set_peak(&mut self, bps: u32) -> Result<(), VideoError> {
        let c = self.component();
        require(c, self.names.peak_bitrate, Variant::int(bps.into()))?;
        optional(c, self.names.vbv_buffer_size, Variant::int(bps.into()))?;
        self.peak_bps = bps;
        Ok(())
    }
}

impl Drop for AmfEncoder {
    fn drop(&mut self) {
        // Terminating the component releases every queued surface (their
        // observer calls run now).
        drop(self.session.take());
        let mut inflight = lock(&self.inflight);
        if inflight.pending.is_empty() {
            drop(inflight);
            // SAFETY: allocated by `Box::leak` in `new`; no surface that
            // could call it is left.
            drop(unsafe { Box::from_raw(self.observer.as_ptr()) });
        } else {
            // The runtime still holds surfaces: keep the observer and the
            // textures alive for good rather than risk a use after free.
            for (_, (_, texture)) in inflight.pending.drain() {
                std::mem::forget(texture);
            }
        }
    }
}

fn validate(p: &AmfParams) -> Result<(), VideoError> {
    let bad = |m: String| Err(VideoError::InvalidConfig(m));
    if p.codec == VideoCodec::H264 && p.color.is_hdr() {
        return bad("HDR10 needs HEVC or AV1; H.264 is 8-bit only here".into());
    }
    if p.width == 0 || p.height == 0 || p.width % 2 != 0 || p.height % 2 != 0 {
        return bad(format!(
            "size {}x{} must be even and non-zero",
            p.width, p.height
        ));
    }
    if p.width > i32::MAX as u32 || p.height > i32::MAX as u32 {
        return bad(format!("size {}x{} is too large", p.width, p.height));
    }
    if !(1..=240).contains(&p.fps) {
        return bad(format!("fps {} is outside 1..=240", p.fps));
    }
    if p.bitrate_kbps == 0 {
        return bad("bitrate must be positive".into());
    }
    Ok(())
}

fn amf_err(context: &str, r: AmfResult) -> VideoError {
    VideoError::Other(format!("AMF {context} failed: {}", result_name(r)))
}

fn check(r: AmfResult, context: &str) -> Result<(), VideoError> {
    if r == sys::AMF_OK {
        Ok(())
    } else {
        Err(amf_err(context, r))
    }
}

fn require<V: StorageVtbl>(obj: &Ptr<V>, name: PCWSTR, value: Variant) -> Result<(), VideoError> {
    match obj.set(name, value) {
        sys::AMF_OK => Ok(()),
        r => Err(rejected(name, r)),
    }
}

/// Like [`require`], but a property this runtime does not know is fine.
fn optional<V: StorageVtbl>(obj: &Ptr<V>, name: PCWSTR, value: Variant) -> Result<(), VideoError> {
    match obj.set(name, value) {
        sys::AMF_OK | sys::AMF_NOT_FOUND => Ok(()),
        r => Err(rejected(name, r)),
    }
}

fn rejected(name: PCWSTR, r: AmfResult) -> VideoError {
    VideoError::Other(format!(
        "AMF property {} rejected: {}",
        prop_name(name),
        result_name(r)
    ))
}

/// An AMF context on our device plus one encoder component; both are
/// terminated and released on drop, whether or not `Init` ran.
struct Session {
    component: ManuallyDrop<Ptr<ComponentVtbl>>,
    context: ManuallyDrop<Ptr<ContextVtbl>>,
}

impl Session {
    fn new(device: &ID3D11Device, codec: VideoCodec) -> Result<Self, VideoError> {
        let rt = runtime()?;
        // SAFETY: the factory is a valid runtime singleton (see `Runtime`).
        let factory = unsafe { &*rt.factory.as_ref().vtbl };
        let mut raw = std::ptr::null_mut();
        // SAFETY: valid factory and out-pointer; returns an owned reference.
        let r = unsafe { (factory.create_context)(rt.factory.as_ptr().cast(), &mut raw) };
        check(r, "CreateContext")?;
        // SAFETY: owned reference to an AMFContext (or null on failure).
        let context: Ptr<ContextVtbl> = unsafe { Ptr::from_raw(raw) }
            .ok_or_else(|| VideoError::Other("AMF CreateContext returned null".into()))?;
        let terminate = |context: &Ptr<ContextVtbl>| {
            // SAFETY: valid context with no surfaces of ours.
            unsafe { (context.vtbl().terminate)(context.raw()) };
        };
        // SAFETY: valid context; the device pointer is a live ID3D11Device
        // that the runtime AddRefs for the context's lifetime.
        let r =
            unsafe { (context.vtbl().init_dx11)(context.raw(), device.as_raw(), sys::AMF_DX11_0) };
        if let Err(e) = check(r, "InitDX11") {
            terminate(&context);
            return Err(e);
        }
        let id = match codec {
            VideoCodec::H264 => props::AVC,
            VideoCodec::Hevc => props::HEVC,
            VideoCodec::Av1 => props::AV1,
        };
        let mut raw = std::ptr::null_mut();
        // SAFETY: valid factory, context and NUL-terminated component id.
        let r = unsafe {
            (factory.create_component)(
                rt.factory.as_ptr().cast(),
                context.0.as_ptr(),
                id.0,
                &mut raw,
            )
        };
        // SAFETY: owned reference to an AMFComponent when non-null.
        match (r, unsafe { Ptr::from_raw(raw) }) {
            (sys::AMF_OK, Some(component)) => Ok(Self {
                component: ManuallyDrop::new(component),
                context: ManuallyDrop::new(context),
            }),
            (r, component) => {
                drop(component);
                terminate(&context);
                Err(VideoError::Other(format!(
                    "no AMD AMF {} encoder on this GPU ({})",
                    codec.name(),
                    result_name(r)
                )))
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: valid objects; `Terminate` is allowed before or after
        // `Init` and errors are irrelevant here. Each field is dropped
        // (released) exactly once, the component before the context.
        unsafe {
            (self.component.vtbl().terminate)(self.component.raw());
            ManuallyDrop::drop(&mut self.component);
            (self.context.vtbl().terminate)(self.context.raw());
            ManuallyDrop::drop(&mut self.context);
        }
    }
}

/// The encoder's capability store.
fn caps(component: &Ptr<ComponentVtbl>) -> Result<Ptr<PropertyStorageVtbl>, VideoError> {
    let mut raw = std::ptr::null_mut();
    // SAFETY: valid component and out-pointer; returns an owned AMFCaps.
    let r = unsafe { (component.vtbl().get_caps)(component.raw(), &mut raw) };
    check(r, "GetCaps")?;
    // SAFETY: owned reference to an AMFCaps (an AMFPropertyStorage).
    unsafe { Ptr::from_raw(raw) }
        .ok_or_else(|| VideoError::Other("AMF GetCaps returned null".into()))
}

/// The AV1 encoder's required width and height multiples (1 when the caps
/// do not say).
fn av1_alignment(component: &Ptr<ComponentVtbl>) -> Result<(u32, u32), VideoError> {
    let caps = caps(component)?;
    let factor = |name| {
        caps.get_i64(name)
            .and_then(|v| u32::try_from(v).ok())
            .filter(|&v| v > 0)
            .unwrap_or(1)
    };
    Ok((
        factor(props::av1::CAP_WIDTH_ALIGNMENT_FACTOR),
        factor(props::av1::CAP_HEIGHT_ALIGNMENT_FACTOR),
    ))
}

/// The multiples the encoded width and height must have for `codec` on
/// `device`'s GPU, so the pipeline can pick (and letterbox into) a size
/// [`AmfEncoder::new`] accepts. H.264 and HEVC code any even size exactly
/// (the bitstream crops the padding). The RDNA3 AV1 encoder needs 64 x 16:
/// on driver 32.0.31041 (AMF 1.5.2) every alignment mode either refuses
/// other sizes or pads them visibly (1080 lines come out as 1082, 946x532 as
/// 960x544, without a render size to crop back).
pub(crate) fn size_alignment(
    device: &ID3D11Device,
    codec: VideoCodec,
) -> Result<(u32, u32), VideoError> {
    match codec {
        VideoCodec::H264 | VideoCodec::Hevc => Ok((2, 2)),
        VideoCodec::Av1 => {
            let session = Session::new(device, codec)?;
            let (w, h) = av1_alignment(&session.component)?;
            Ok((w.max(2), h.max(2)))
        }
    }
}

/// Sets every static and initial dynamic property before `Init`. Usage goes
/// first: it resets the others to the usage's defaults.
fn configure(
    c: &Ptr<ComponentVtbl>,
    n: &Names,
    p: &AmfParams,
    mean: u32,
    peak: u32,
) -> Result<(), VideoError> {
    use props::{av1, avc, hevc};
    let hdr = p.color.is_hdr();
    let int = |v: i64| Variant::int(v);
    let fps = i64::from(p.fps);

    require(c, n.usage, int(n.usage_low_latency))?;
    let profile = match (p.codec, hdr) {
        (VideoCodec::H264, _) => avc::PROFILE_HIGH,
        (VideoCodec::Hevc, false) => hevc::PROFILE_MAIN,
        (VideoCodec::Hevc, true) => hevc::PROFILE_MAIN_10,
        (VideoCodec::Av1, _) => av1::PROFILE_MAIN,
    };
    require(c, n.profile, int(profile))?;
    require(c, n.quality_preset, int(n.preset_balanced))?;
    require(
        c,
        n.frame_size,
        Variant::size(p.width as i32, p.height as i32),
    )?;
    require(c, n.frame_rate, Variant::rate(p.fps, 1))?;
    // Never block in QueryOutput.
    require(c, n.query_timeout, int(0))?;

    // GOP: a keyframe every second with headers; no B-frames. Keyframes are
    // also forced per frame in `submit`, at the same cadence.
    match p.codec {
        VideoCodec::H264 => {
            require(c, avc::IDR_PERIOD, int(fps))?;
            require(c, avc::HEADER_INSERTION_SPACING, int(fps))?;
            optional(c, avc::B_PIC_PATTERN, int(0))?;
            optional(c, avc::MAX_CONSECUTIVE_BPICTURES, int(0))?;
        }
        VideoCodec::Hevc => {
            require(c, hevc::GOP_SIZE, int(fps))?;
            require(c, hevc::NUM_GOPS_PER_IDR, int(1))?;
            require(
                c,
                hevc::HEADER_INSERTION_MODE,
                int(hevc::HEADER_INSERTION_MODE_IDR_ALIGNED),
            )?;
        }
        VideoCodec::Av1 => {
            require(c, av1::GOP_SIZE, int(fps))?;
            require(
                c,
                av1::HEADER_INSERTION_MODE,
                int(av1::HEADER_INSERTION_MODE_KEY_FRAME_ALIGNED),
            )?;
            optional(c, av1::B_PIC_PATTERN, int(0))?;
            optional(c, av1::MAX_CONSECUTIVE_BPICTURES, int(0))?;
            // Without this, RDNA3 codes 1080 lines as 1082 (64x16 blocks).
            require(
                c,
                av1::ALIGNMENT_MODE,
                int(av1::ALIGNMENT_MODE_NO_RESTRICTIONS),
            )?;
        }
    }

    // Rate control: peak-constrained VBR, VBV one second at the peak.
    require(c, n.rate_control, int(RATE_CONTROL_PEAK_CONSTRAINED_VBR))?;
    require(c, n.target_bitrate, int(mean.into()))?;
    require(c, n.peak_bitrate, int(peak.into()))?;
    optional(c, n.vbv_buffer_size, int(peak.into()))?;
    // A skipped frame would break the one-output-per-input contract.
    optional(c, n.skip_frame, Variant::bool(false))?;

    // Colour: what the bitstream signals (VUI / sequence header), limited
    // range. The input side is the same (no conversion inside AMF).
    let (depth, profile, transfer, primaries) = if hdr {
        (
            10,
            props::COLOR_PROFILE_2020,
            props::TRANSFER_SMPTE2084,
            props::PRIMARIES_BT2020,
        )
    } else {
        (
            8,
            props::COLOR_PROFILE_709,
            props::TRANSFER_BT709,
            props::PRIMARIES_BT709,
        )
    };
    if p.codec == VideoCodec::H264 {
        optional(c, n.bit_depth, int(depth))?;
    } else {
        require(c, n.bit_depth, int(depth))?;
    }
    require(c, n.out_profile, int(profile))?;
    require(c, n.out_transfer, int(transfer))?;
    require(c, n.out_primaries, int(primaries))?;
    optional(c, n.in_profile, int(profile))?;
    optional(c, n.in_transfer, int(transfer))?;
    optional(c, n.in_primaries, int(primaries))?;
    optional(c, n.out_full_range, Variant::bool(false))?;
    optional(c, n.in_full_range, Variant::bool(false))?;
    Ok(())
}

/// The rate control and GOP the encoder reports after `Init`.
fn describe(c: &Ptr<ComponentVtbl>, n: &Names, codec: VideoCodec) -> String {
    use props::{av1, avc, hevc};
    let show = |v: Option<i64>| v.map_or_else(|| "?".to_string(), |v| v.to_string());
    let gop = match codec {
        VideoCodec::H264 => c.get_i64(avc::IDR_PERIOD),
        VideoCodec::Hevc => c.get_i64(hevc::GOP_SIZE),
        VideoCodec::Av1 => c.get_i64(av1::GOP_SIZE),
    };
    let b_frames = match codec {
        VideoCodec::H264 => c.get_i64(avc::MAX_CONSECUTIVE_BPICTURES),
        VideoCodec::Hevc => Some(0),
        VideoCodec::Av1 => c.get_i64(av1::MAX_CONSECUTIVE_BPICTURES),
    };
    format!(
        "{} (mean {} bps, max {} bps, VBV {} bits, GOP {}, B-frames {}, preset {})",
        c.get_i64(n.rate_control)
            .map_or("unreported", |m| rate_control_name(codec, m)),
        show(c.get_i64(n.target_bitrate)),
        show(c.get_i64(n.peak_bitrate)),
        show(c.get_i64(n.vbv_buffer_size)),
        show(gop),
        show(b_frames),
        // Each codec numbers its presets differently; only ours gets a name.
        match c.get_i64(n.quality_preset) {
            Some(p) if p == n.preset_balanced => "balanced".to_string(),
            other => show(other),
        },
    )
}
