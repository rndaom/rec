//! Minimal hand-written bindings for the subset of the AMD Advanced Media
//! Framework runtime (`amfrt64.dll`) that the encoder uses.
//!
//! Layouts, vtable orders, enum values and property names follow the AMF SDK
//! headers (MIT, Advanced Micro Devices): `core/{Interface,PropertyStorage,
//! Variant,Data,Buffer,Surface,Context,Factory,Result,Version,Platform}.h` and
//! `components/{Component,ComponentCaps,ColorSpace,VideoEncoderVCE,
//! VideoEncoderHEVC,VideoEncoderAV1}.h`. The SDK's C branch (`#ifndef
//! __cplusplus`) declares every interface as a struct holding a pointer to a
//! table of function pointers in the C++ declaration order; the runtime's
//! objects are C++ objects with exactly that vtable layout (no interface used
//! here has overloaded virtual methods, so MSVC keeps declaration order).
//!
//! Only the vtable entries we call have function-pointer types; the others are
//! `usize` placeholders named after the SDK member, and the compile-time
//! `offset_of!` checks at the bottom pin every entry we call to its index.
//! Everything here is raw; `super` wraps it.

use std::ffi::c_void;

/// `AMF_RESULT`.
pub(super) type AmfResult = i32;

pub(super) const AMF_OK: AmfResult = 0;
pub(super) const AMF_NOT_FOUND: AmfResult = 11;
pub(super) const AMF_EOF: AmfResult = 23;
pub(super) const AMF_REPEAT: AmfResult = 24;
pub(super) const AMF_INPUT_FULL: AmfResult = 25;
pub(super) const AMF_NEED_MORE_INPUT: AmfResult = 44;

/// Names of the `AMF_RESULT` values (`core/Result.h`, consecutive from 0).
pub(super) const RESULT_NAMES: [&str; 46] = [
    "AMF_OK",
    "AMF_FAIL",
    "AMF_UNEXPECTED",
    "AMF_ACCESS_DENIED",
    "AMF_INVALID_ARG",
    "AMF_OUT_OF_RANGE",
    "AMF_OUT_OF_MEMORY",
    "AMF_INVALID_POINTER",
    "AMF_NO_INTERFACE",
    "AMF_NOT_IMPLEMENTED",
    "AMF_NOT_SUPPORTED",
    "AMF_NOT_FOUND",
    "AMF_ALREADY_INITIALIZED",
    "AMF_NOT_INITIALIZED",
    "AMF_INVALID_FORMAT",
    "AMF_WRONG_STATE",
    "AMF_FILE_NOT_OPEN",
    "AMF_NO_DEVICE",
    "AMF_DIRECTX_FAILED",
    "AMF_OPENCL_FAILED",
    "AMF_GLX_FAILED",
    "AMF_XV_FAILED",
    "AMF_ALSA_FAILED",
    "AMF_EOF",
    "AMF_REPEAT",
    "AMF_INPUT_FULL",
    "AMF_RESOLUTION_CHANGED",
    "AMF_RESOLUTION_UPDATED",
    "AMF_INVALID_DATA_TYPE",
    "AMF_INVALID_RESOLUTION",
    "AMF_CODEC_NOT_SUPPORTED",
    "AMF_SURFACE_FORMAT_NOT_SUPPORTED",
    "AMF_SURFACE_MUST_BE_SHARED",
    "AMF_DECODER_NOT_PRESENT",
    "AMF_DECODER_SURFACE_ALLOCATION_FAILED",
    "AMF_DECODER_NO_FREE_SURFACES",
    "AMF_ENCODER_NOT_PRESENT",
    "AMF_DEM_ERROR",
    "AMF_DEM_PROPERTY_READONLY",
    "AMF_DEM_REMOTE_DISPLAY_CREATE_FAILED",
    "AMF_DEM_START_ENCODING_FAILED",
    "AMF_DEM_QUERY_OUTPUT_FAILED",
    "AMF_TAN_CLIPPING_WAS_REQUIRED",
    "AMF_TAN_UNSUPPORTED_VERSION",
    "AMF_NEED_MORE_INPUT",
    "AMF_VULKAN_FAILED",
];

/// `AMF_MAKE_FULL_VERSION`: 16 bits each of major, minor, release, build.
pub(super) const fn make_version(major: u64, minor: u64, release: u64, build: u64) -> u64 {
    (major << 48) | (minor << 32) | (release << 16) | build
}

/// `AMF_SURFACE_FORMAT`.
pub(super) const AMF_SURFACE_NV12: i32 = 1;
pub(super) const AMF_SURFACE_P010: i32 = 10;

/// `AMF_MEMORY_TYPE`.
pub(super) const AMF_MEMORY_HOST: i32 = 1;

/// `AMF_DX_VERSION`.
pub(super) const AMF_DX11_0: i32 = 110;

/// `AMF_VARIANT_TYPE`.
pub(super) const AMF_VARIANT_EMPTY: i32 = 0;
pub(super) const AMF_VARIANT_BOOL: i32 = 1;
pub(super) const AMF_VARIANT_INT64: i32 = 2;
pub(super) const AMF_VARIANT_SIZE: i32 = 5;
pub(super) const AMF_VARIANT_RATE: i32 = 7;

/// `AMFGuid`: same layout as a Windows GUID.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(super) struct AmfGuid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

/// `AMF_DECLARE_IID(AMFBuffer, ...)`.
pub(super) const IID_AMF_BUFFER: AmfGuid = AmfGuid {
    data1: 0xb04b_7248,
    data2: 0xb6f0,
    data3: 0x4321,
    data4: [0xb6, 0x91, 0xba, 0xa4, 0x74, 0x0f, 0x9f, 0xcb],
};

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(super) struct AmfSize {
    pub width: i32,
    pub height: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(super) struct AmfRate {
    pub num: u32,
    pub den: u32,
}

/// The anonymous union of `AMFVariantStruct`. Its largest members
/// (`AMFRect`, `AMFFloatVector4D`) are 16 bytes and it holds pointers and
/// 64-bit values, hence 16 bytes aligned to 8.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) union VariantValue {
    pub bool_value: u8,
    pub int64_value: i64,
    pub size_value: AmfSize,
    pub rate_value: AmfRate,
    pub raw: [u64; 2],
}

/// `AMFVariantStruct`. Only plain-value variants are built or read here;
/// string and interface variants own memory (`AMFVariantClear`) and are never
/// requested.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Variant {
    pub kind: i32,
    pub value: VariantValue,
}

impl Variant {
    pub(super) fn empty() -> Self {
        Self {
            kind: AMF_VARIANT_EMPTY,
            value: VariantValue { raw: [0; 2] },
        }
    }

    pub(super) fn int(v: i64) -> Self {
        Self {
            kind: AMF_VARIANT_INT64,
            value: VariantValue { int64_value: v },
        }
    }

    pub(super) fn bool(v: bool) -> Self {
        let mut value = VariantValue { raw: [0; 2] };
        value.bool_value = u8::from(v);
        Self {
            kind: AMF_VARIANT_BOOL,
            value,
        }
    }

    pub(super) fn size(width: i32, height: i32) -> Self {
        let mut value = VariantValue { raw: [0; 2] };
        value.size_value = AmfSize { width, height };
        Self {
            kind: AMF_VARIANT_SIZE,
            value,
        }
    }

    pub(super) fn rate(num: u32, den: u32) -> Self {
        let mut value = VariantValue { raw: [0; 2] };
        value.rate_value = AmfRate { num, den };
        Self {
            kind: AMF_VARIANT_RATE,
            value,
        }
    }

    /// The value as an integer, for the integer and boolean variants.
    pub(super) fn as_i64(&self) -> Option<i64> {
        // SAFETY: the active union member is the one `kind` names.
        unsafe {
            match self.kind {
                AMF_VARIANT_INT64 => Some(self.value.int64_value),
                AMF_VARIANT_BOOL => Some(i64::from(self.value.bool_value != 0)),
                _ => None,
            }
        }
    }
}

/// Any AMF object: a pointer to its vtable (`struct AMFInterface { const
/// AMFInterfaceVtbl* pVtbl; }` and every derived interface).
#[repr(C)]
pub(super) struct Object<V> {
    pub vtbl: *const V,
}

type This = *mut c_void;

/// `AMFInterfaceVtbl`.
#[repr(C)]
pub(super) struct InterfaceVtbl {
    pub acquire: unsafe extern "system" fn(This) -> i32,
    pub release: unsafe extern "system" fn(This) -> i32,
    pub query_interface:
        unsafe extern "system" fn(This, *const AmfGuid, *mut *mut c_void) -> AmfResult,
}

/// `AMFPropertyStorageVtbl` (AMFInterface + AMFPropertyStorage).
#[repr(C)]
pub(super) struct PropertyStorageVtbl {
    pub base: InterfaceVtbl,
    pub set_property: unsafe extern "system" fn(This, *const u16, Variant) -> AmfResult,
    pub get_property: unsafe extern "system" fn(This, *const u16, *mut Variant) -> AmfResult,
    pub has_property: usize,
    pub get_property_count: usize,
    pub get_property_at: usize,
    pub clear: usize,
    pub add_to: usize,
    pub copy_to: usize,
    pub add_observer: usize,
    pub remove_observer: usize,
}

/// `AMFFactoryVtbl`. `AMFFactory` is not an `AMFInterface`: it is a process
/// singleton owned by the runtime and is never released.
#[repr(C)]
pub(super) struct FactoryVtbl {
    pub create_context: unsafe extern "system" fn(This, *mut *mut Object<ContextVtbl>) -> AmfResult,
    pub create_component: unsafe extern "system" fn(
        This,
        *mut Object<ContextVtbl>,
        *const u16,
        *mut *mut Object<ComponentVtbl>,
    ) -> AmfResult,
    pub set_cache_folder: usize,
    pub get_cache_folder: usize,
    pub get_debug: usize,
    pub get_trace: usize,
    pub get_programs: usize,
}

/// `AMFContextVtbl`, up to `CreateSurfaceFromDX11Native`.
#[repr(C)]
pub(super) struct ContextVtbl {
    pub storage: PropertyStorageVtbl,
    pub terminate: unsafe extern "system" fn(This) -> AmfResult,
    pub init_dx9: usize,
    pub get_dx9_device: usize,
    pub lock_dx9: usize,
    pub unlock_dx9: usize,
    pub init_dx11: unsafe extern "system" fn(This, *mut c_void, i32) -> AmfResult,
    pub get_dx11_device: usize,
    pub lock_dx11: usize,
    pub unlock_dx11: usize,
    /// InitOpenCL, GetOpenCLContext, GetOpenCLCommandQueue,
    /// GetOpenCLDeviceID, GetOpenCLComputeFactory, InitOpenCLEx, LockOpenCL,
    /// UnlockOpenCL.
    pub opencl: [usize; 8],
    /// InitOpenGL, GetOpenGLContext, GetOpenGLDrawable, LockOpenGL,
    /// UnlockOpenGL.
    pub opengl: [usize; 5],
    /// InitXV, GetXVDevice, LockXV, UnlockXV.
    pub xv: [usize; 4],
    /// InitGralloc, GetGrallocDevice, LockGralloc, UnlockGralloc.
    pub gralloc: [usize; 4],
    pub alloc_buffer: usize,
    pub alloc_surface: usize,
    pub alloc_audio_buffer: usize,
    pub create_buffer_from_host_native: usize,
    pub create_surface_from_host_native: usize,
    pub create_surface_from_dx9_native: usize,
    pub create_surface_from_dx11_native: unsafe extern "system" fn(
        This,
        *mut c_void,
        *mut *mut Object<DataVtbl>,
        *mut SurfaceObserver,
    ) -> AmfResult,
}

/// `AMFComponentVtbl` (AMFPropertyStorageEx in between).
#[repr(C)]
pub(super) struct ComponentVtbl {
    pub storage: PropertyStorageVtbl,
    pub get_properties_info_count: usize,
    pub get_property_info_at: usize,
    pub get_property_info: usize,
    pub validate_property: usize,
    pub init: unsafe extern "system" fn(This, i32, i32, i32) -> AmfResult,
    pub reinit: usize,
    pub terminate: unsafe extern "system" fn(This) -> AmfResult,
    pub drain: usize,
    pub flush: usize,
    pub submit_input: unsafe extern "system" fn(This, *mut c_void) -> AmfResult,
    pub query_output: unsafe extern "system" fn(This, *mut *mut Object<DataVtbl>) -> AmfResult,
    pub get_context: usize,
    pub set_output_data_allocator_cb: usize,
    pub get_caps:
        unsafe extern "system" fn(This, *mut *mut Object<PropertyStorageVtbl>) -> AmfResult,
    pub optimize: usize,
}

/// `AMFDataVtbl`; also the prefix of `AMFSurfaceVtbl`, which is all a
/// surface is used through here.
#[repr(C)]
pub(super) struct DataVtbl {
    pub storage: PropertyStorageVtbl,
    pub get_memory_type: unsafe extern "system" fn(This) -> i32,
    pub duplicate: usize,
    pub convert: unsafe extern "system" fn(This, i32) -> AmfResult,
    pub interop: usize,
    pub get_data_type: usize,
    pub is_reusable: usize,
    pub set_pts: unsafe extern "system" fn(This, i64),
    pub get_pts: unsafe extern "system" fn(This) -> i64,
    pub set_duration: unsafe extern "system" fn(This, i64),
    pub get_duration: usize,
}

/// `AMFBufferVtbl`, up to `GetNative`.
#[repr(C)]
pub(super) struct BufferVtbl {
    pub data: DataVtbl,
    pub set_size: usize,
    pub get_size: unsafe extern "system" fn(This) -> usize,
    pub get_native: unsafe extern "system" fn(This) -> *mut c_void,
}

/// `AMFSurfaceObserverVtbl`.
#[repr(C)]
pub(super) struct SurfaceObserverVtbl {
    pub on_surface_data_release: unsafe extern "system" fn(*mut SurfaceObserver, *mut c_void),
}

/// `AMFSurfaceObserver`: implemented by us, called by the runtime. Our
/// implementation embeds this as the first field of a larger struct.
#[repr(C)]
pub(super) struct SurfaceObserver {
    pub vtbl: *const SurfaceObserverVtbl,
}

/// `AMFQueryVersion_Fn` (`AMF_CDECL_CALL`).
pub(super) type QueryVersionFn = unsafe extern "C" fn(*mut u64) -> AmfResult;
/// `AMFInit_Fn` (`AMF_CDECL_CALL`).
pub(super) type InitFn = unsafe extern "C" fn(u64, *mut *mut Object<FactoryVtbl>) -> AmfResult;

/// Every interface we call through starts with the AMFInterface and
/// AMFPropertyStorage tables.
///
/// # Safety
/// Implementors must be `#[repr(C)]` vtables whose first field is a
/// [`PropertyStorageVtbl`].
pub(super) unsafe trait StorageVtbl {
    fn storage(&self) -> &PropertyStorageVtbl;
}

// SAFETY (all four): `#[repr(C)]` with a `PropertyStorageVtbl` first.
unsafe impl StorageVtbl for PropertyStorageVtbl {
    fn storage(&self) -> &PropertyStorageVtbl {
        self
    }
}
// SAFETY: see above.
unsafe impl StorageVtbl for ContextVtbl {
    fn storage(&self) -> &PropertyStorageVtbl {
        &self.storage
    }
}
// SAFETY: see above.
unsafe impl StorageVtbl for ComponentVtbl {
    fn storage(&self) -> &PropertyStorageVtbl {
        &self.storage
    }
}
// SAFETY: see above.
unsafe impl StorageVtbl for DataVtbl {
    fn storage(&self) -> &PropertyStorageVtbl {
        &self.storage
    }
}
// SAFETY: see above.
unsafe impl StorageVtbl for BufferVtbl {
    fn storage(&self) -> &PropertyStorageVtbl {
        &self.data.storage
    }
}

/// Property names and values from the encoder component headers.
pub(super) mod props {
    use windows::core::{PCWSTR, w};

    /// Component ids.
    pub const AVC: PCWSTR = w!("AMFVideoEncoderVCE_AVC");
    pub const HEVC: PCWSTR = w!("AMFVideoEncoderHW_HEVC");
    pub const AV1: PCWSTR = w!("AMFVideoEncoderHW_AV1");

    /// `ColorSpace.h`: `AMF_COLOR_BIT_DEPTH_ENUM`,
    /// `AMF_VIDEO_CONVERTER_COLOR_PROFILE_ENUM` (limited range),
    /// `AMF_COLOR_PRIMARIES_ENUM`, `AMF_COLOR_TRANSFER_CHARACTERISTIC_ENUM`.
    /// Primaries and transfer use the H.273 code points.
    pub const COLOR_PROFILE_709: i64 = 1;
    pub const COLOR_PROFILE_2020: i64 = 2;
    pub const PRIMARIES_BT709: i64 = 1;
    pub const PRIMARIES_BT2020: i64 = 9;
    pub const TRANSFER_BT709: i64 = 1;
    pub const TRANSFER_SMPTE2084: i64 = 16;

    /// `VideoEncoderVCE.h` (H.264).
    pub mod avc {
        use windows::core::{PCWSTR, w};
        pub const USAGE: PCWSTR = w!("Usage");
        pub const USAGE_LOW_LATENCY: i64 = 2;
        pub const PROFILE: PCWSTR = w!("Profile");
        pub const PROFILE_HIGH: i64 = 100;
        pub const QUALITY_PRESET: PCWSTR = w!("QualityPreset");
        pub const QUALITY_PRESET_BALANCED: i64 = 0;
        pub const FRAMESIZE: PCWSTR = w!("FrameSize");
        pub const FRAMERATE: PCWSTR = w!("FrameRate");
        pub const B_PIC_PATTERN: PCWSTR = w!("BPicturesPattern");
        pub const MAX_CONSECUTIVE_BPICTURES: PCWSTR = w!("MaxConsecutiveBPictures");
        pub const IDR_PERIOD: PCWSTR = w!("IDRPeriod");
        pub const HEADER_INSERTION_SPACING: PCWSTR = w!("HeaderInsertionSpacing");
        pub const RATE_CONTROL_METHOD: PCWSTR = w!("RateControlMethod");
        pub const TARGET_BITRATE: PCWSTR = w!("TargetBitrate");
        pub const PEAK_BITRATE: PCWSTR = w!("PeakBitrate");
        pub const VBV_BUFFER_SIZE: PCWSTR = w!("VBVBufferSize");
        pub const RATE_CONTROL_SKIP_FRAME_ENABLE: PCWSTR = w!("RateControlSkipFrameEnable");
        pub const QUERY_TIMEOUT: PCWSTR = w!("QueryTimeout");
        pub const COLOR_BIT_DEPTH: PCWSTR = w!("ColorBitDepth");
        pub const OUTPUT_FULL_RANGE_COLOR: PCWSTR = w!("FullRangeColor");
        pub const INPUT_FULL_RANGE_COLOR: PCWSTR = w!("InputFullRangeColor");
        pub const INPUT_COLOR_PROFILE: PCWSTR = w!("InColorProfile");
        pub const OUTPUT_COLOR_PROFILE: PCWSTR = w!("OutColorProfile");
        pub const INPUT_TRANSFER_CHARACTERISTIC: PCWSTR = w!("InColorTransferChar");
        pub const OUTPUT_TRANSFER_CHARACTERISTIC: PCWSTR = w!("OutColorTransferChar");
        pub const INPUT_COLOR_PRIMARIES: PCWSTR = w!("InColorPrimaries");
        pub const OUTPUT_COLOR_PRIMARIES: PCWSTR = w!("OutColorPrimaries");
        pub const FORCE_PICTURE_TYPE: PCWSTR = w!("ForcePictureType");
        pub const PICTURE_TYPE_IDR: i64 = 2;
        pub const INSERT_SPS: PCWSTR = w!("InsertSPS");
        pub const INSERT_PPS: PCWSTR = w!("InsertPPS");
        pub const OUTPUT_DATA_TYPE: PCWSTR = w!("OutputDataType");
        pub const OUTPUT_DATA_TYPE_IDR: i64 = 0;
    }

    /// `VideoEncoderHEVC.h`.
    pub mod hevc {
        use windows::core::{PCWSTR, w};
        pub const USAGE: PCWSTR = w!("HevcUsage");
        pub const USAGE_LOW_LATENCY: i64 = 2;
        pub const PROFILE: PCWSTR = w!("HevcProfile");
        pub const PROFILE_MAIN: i64 = 1;
        pub const PROFILE_MAIN_10: i64 = 2;
        pub const QUALITY_PRESET: PCWSTR = w!("HevcQualityPreset");
        pub const QUALITY_PRESET_BALANCED: i64 = 5;
        pub const FRAMESIZE: PCWSTR = w!("HevcFrameSize");
        pub const FRAMERATE: PCWSTR = w!("HevcFrameRate");
        pub const GOP_SIZE: PCWSTR = w!("HevcGOPSize");
        pub const NUM_GOPS_PER_IDR: PCWSTR = w!("HevcGOPSPerIDR");
        pub const HEADER_INSERTION_MODE: PCWSTR = w!("HevcHeaderInsertionMode");
        pub const HEADER_INSERTION_MODE_IDR_ALIGNED: i64 = 2;
        pub const RATE_CONTROL_METHOD: PCWSTR = w!("HevcRateControlMethod");
        pub const TARGET_BITRATE: PCWSTR = w!("HevcTargetBitrate");
        pub const PEAK_BITRATE: PCWSTR = w!("HevcPeakBitrate");
        pub const VBV_BUFFER_SIZE: PCWSTR = w!("HevcVBVBufferSize");
        pub const RATE_CONTROL_SKIP_FRAME_ENABLE: PCWSTR = w!("HevcRateControlSkipFrameEnable");
        pub const QUERY_TIMEOUT: PCWSTR = w!("HevcQueryTimeout");
        pub const COLOR_BIT_DEPTH: PCWSTR = w!("HevcColorBitDepth");
        pub const OUTPUT_FULL_RANGE_COLOR: PCWSTR = w!("HevcNominalRange");
        pub const INPUT_FULL_RANGE_COLOR: PCWSTR = w!("HevcInputFullRangeColor");
        pub const INPUT_COLOR_PROFILE: PCWSTR = w!("HevcInColorProfile");
        pub const OUTPUT_COLOR_PROFILE: PCWSTR = w!("HevcOutColorProfile");
        pub const INPUT_TRANSFER_CHARACTERISTIC: PCWSTR = w!("HevcInColorTransferChar");
        pub const OUTPUT_TRANSFER_CHARACTERISTIC: PCWSTR = w!("HevcOutColorTransferChar");
        pub const INPUT_COLOR_PRIMARIES: PCWSTR = w!("HevcInColorPrimaries");
        pub const OUTPUT_COLOR_PRIMARIES: PCWSTR = w!("HevcOutColorPrimaries");
        pub const FORCE_PICTURE_TYPE: PCWSTR = w!("HevcForcePictureType");
        pub const PICTURE_TYPE_IDR: i64 = 2;
        pub const INSERT_HEADER: PCWSTR = w!("HevcInsertHeader");
        pub const OUTPUT_DATA_TYPE: PCWSTR = w!("HevcOutputDataType");
        pub const OUTPUT_DATA_TYPE_IDR: i64 = 0;
        pub const CAP_MAX_PROFILE: PCWSTR = w!("HevcMaxProfile");
    }

    /// `VideoEncoderAV1.h`.
    pub mod av1 {
        use windows::core::{PCWSTR, w};
        pub const USAGE: PCWSTR = w!("Av1Usage");
        /// Unlike H.264/HEVC, LOW_LATENCY is 1 and ULTRA_LOW_LATENCY 2.
        pub const USAGE_LOW_LATENCY: i64 = 1;
        pub const PROFILE: PCWSTR = w!("Av1Profile");
        pub const PROFILE_MAIN: i64 = 1;
        pub const QUALITY_PRESET: PCWSTR = w!("Av1QualityPreset");
        pub const QUALITY_PRESET_BALANCED: i64 = 70;
        pub const FRAMESIZE: PCWSTR = w!("Av1FrameSize");
        pub const FRAMERATE: PCWSTR = w!("Av1FrameRate");
        pub const ALIGNMENT_MODE: PCWSTR = w!("Av1AlignmentMode");
        pub const ALIGNMENT_MODE_NO_RESTRICTIONS: i64 = 3;
        pub const GOP_SIZE: PCWSTR = w!("Av1GOPSize");
        pub const HEADER_INSERTION_MODE: PCWSTR = w!("Av1HeaderInsertionMode");
        pub const HEADER_INSERTION_MODE_KEY_FRAME_ALIGNED: i64 = 2;
        pub const B_PIC_PATTERN: PCWSTR = w!("Av1BPicturesPattern");
        pub const MAX_CONSECUTIVE_BPICTURES: PCWSTR = w!("Av1MaxConsecutiveBPictures");
        pub const RATE_CONTROL_METHOD: PCWSTR = w!("Av1RateControlMethod");
        pub const TARGET_BITRATE: PCWSTR = w!("Av1TargetBitrate");
        pub const PEAK_BITRATE: PCWSTR = w!("Av1PeakBitrate");
        pub const VBV_BUFFER_SIZE: PCWSTR = w!("Av1VBVBufferSize");
        pub const RATE_CONTROL_SKIP_FRAME: PCWSTR = w!("Av1RateControlSkipFrameEnable");
        pub const QUERY_TIMEOUT: PCWSTR = w!("Av1QueryTimeout");
        pub const COLOR_BIT_DEPTH: PCWSTR = w!("Av1ColorBitDepth");
        pub const OUTPUT_FULL_RANGE_COLOR: PCWSTR = w!("Av1NominalRange");
        pub const INPUT_FULL_RANGE_COLOR: PCWSTR = w!("Av1InputFullRangeColor");
        pub const INPUT_COLOR_PROFILE: PCWSTR = w!("Av1InputColorProfile");
        pub const OUTPUT_COLOR_PROFILE: PCWSTR = w!("Av1OutputColorProfile");
        pub const INPUT_TRANSFER_CHARACTERISTIC: PCWSTR = w!("Av1InputColorTransferChar");
        pub const OUTPUT_TRANSFER_CHARACTERISTIC: PCWSTR = w!("Av1OutputColorTransferChar");
        pub const INPUT_COLOR_PRIMARIES: PCWSTR = w!("Av1InputColorPrimaries");
        pub const OUTPUT_COLOR_PRIMARIES: PCWSTR = w!("Av1OutputColorPrimaries");
        pub const FORCE_FRAME_TYPE: PCWSTR = w!("Av1ForceFrameType");
        pub const FORCE_FRAME_TYPE_KEY: i64 = 1;
        pub const FORCE_INSERT_SEQUENCE_HEADER: PCWSTR = w!("Av1ForceInsertSequenceHeader");
        pub const OUTPUT_FRAME_TYPE: PCWSTR = w!("Av1OutputFrameType");
        pub const OUTPUT_FRAME_TYPE_KEY: i64 = 0;
        pub const CAP_WIDTH_ALIGNMENT_FACTOR: PCWSTR = w!("Av1WidthAlignmentFactor");
        pub const CAP_HEIGHT_ALIGNMENT_FACTOR: PCWSTR = w!("Av1HeightAlignmentFactor");
    }
}

// Layout checks against the SDK headers (x64): every called vtable entry at
// its declaration index, and the value structs at their C sizes.
const P: usize = size_of::<usize>();
const _: () = {
    use std::mem::offset_of;
    assert!(size_of::<AmfGuid>() == 16);
    assert!(size_of::<AmfSize>() == 8 && size_of::<AmfRate>() == 8);
    assert!(size_of::<VariantValue>() == 16 && align_of::<VariantValue>() == 8);
    assert!(size_of::<Variant>() == 24 && offset_of!(Variant, value) == 8);

    assert!(size_of::<InterfaceVtbl>() == 3 * P);
    assert!(size_of::<PropertyStorageVtbl>() == 13 * P);
    assert!(offset_of!(PropertyStorageVtbl, set_property) == 3 * P);
    assert!(offset_of!(PropertyStorageVtbl, get_property) == 4 * P);

    assert!(size_of::<FactoryVtbl>() == 7 * P);
    assert!(offset_of!(FactoryVtbl, create_component) == P);

    assert!(offset_of!(ContextVtbl, terminate) == 13 * P);
    assert!(offset_of!(ContextVtbl, init_dx11) == 18 * P);
    assert!(offset_of!(ContextVtbl, alloc_buffer) == 43 * P);
    assert!(offset_of!(ContextVtbl, create_surface_from_dx11_native) == 49 * P);

    assert!(offset_of!(ComponentVtbl, init) == 17 * P);
    assert!(offset_of!(ComponentVtbl, terminate) == 19 * P);
    assert!(offset_of!(ComponentVtbl, submit_input) == 22 * P);
    assert!(offset_of!(ComponentVtbl, query_output) == 23 * P);
    assert!(offset_of!(ComponentVtbl, get_caps) == 26 * P);
    assert!(size_of::<ComponentVtbl>() == 28 * P);

    assert!(offset_of!(DataVtbl, get_memory_type) == 13 * P);
    assert!(offset_of!(DataVtbl, convert) == 15 * P);
    assert!(offset_of!(DataVtbl, set_pts) == 19 * P);
    assert!(offset_of!(DataVtbl, get_pts) == 20 * P);
    assert!(offset_of!(DataVtbl, set_duration) == 21 * P);
    assert!(size_of::<DataVtbl>() == 23 * P);

    assert!(offset_of!(BufferVtbl, get_size) == 24 * P);
    assert!(offset_of!(BufferVtbl, get_native) == 25 * P);
};
