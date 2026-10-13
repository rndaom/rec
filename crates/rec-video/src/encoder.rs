//! Media Foundation hardware encoder MFT (asynchronous), driven by its event
//! generator without blocking.

use std::mem::ManuallyDrop;

use rec_core::media::VideoCodec;
use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMaxBitRate, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount,
    CODECAPI_AVEncMPVGOPSize, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate, IMFAttributes,
    IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFShutdown,
    IMFTransform, MEError, METransformHaveOutput, METransformNeedInput, MF_E_NO_EVENTS_AVAILABLE,
    MF_E_NOTACCEPTING, MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT, MF_EVENT_TYPE,
    MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES,
    MF_MT_YUV_MATRIX, MF_SA_D3D11_AWARE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
    MFCreateAttributes, MFCreateDXGIDeviceManager, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFMediaType_Video, MFNominalRange_16_235, MFSampleExtension_CleanPoint,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_ADAPTER_LUID, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFVideoPrimaries_BT709, MFVideoTransFunc_709,
    MFVideoTransferMatrix_BT709, eAVEncCommonRateControlMode_CBR,
    eAVEncCommonRateControlMode_PeakConstrainedVBR, eAVEncH264VProfile_High,
    eAVEncH265VProfile_Main_420_8,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Interface, PWSTR};

use crate::util::{Context, win_err};
use crate::{CodecPreference, VideoError};

pub(crate) struct Encoder {
    activate: IMFActivate,
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    _manager: IMFDXGIDeviceManager,
    /// Kept for in-place rate changes ([`Encoder::set_bitrate`]).
    api: ICodecAPI,
    /// Mean/peak currently set on the MFT in bit/s, updated after every
    /// successful `SetValue` so the peak >= mean ordering in `set_bitrate`
    /// stays right even after a partial failure.
    mean_bps: u32,
    peak_bps: u32,
    pub name: String,
    pub codec: VideoCodec,
    /// Rate-control description as read back right after configuration; it
    /// is not updated by [`Encoder::set_bitrate`].
    pub rate_control: String,
    /// Annex-B parameter sets from `MF_MT_MPEG_SEQUENCE_HEADER`, if given.
    pub sequence_header: Option<Vec<u8>>,
    input_id: u32,
    output_id: u32,
    provides_samples: bool,
    output_size: u32,
    /// `METransformNeedInput` events not yet answered with `ProcessInput`.
    need_input: u32,
    /// Reused output buffer when the MFT does not allocate its own.
    output_sample: Option<IMFSample>,
}

impl std::fmt::Debug for Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Encoder")
            .field("name", &self.name)
            .field("codec", &self.codec)
            .field("rate_control", &self.rate_control)
            .finish_non_exhaustive()
    }
}

pub(crate) struct EncoderParams {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

fn subtype(codec: VideoCodec) -> GUID {
    match codec {
        VideoCodec::Hevc => MFVideoFormat_HEVC,
        _ => MFVideoFormat_H264,
    }
}

fn pack(hi: u32, lo: u32) -> u64 {
    (u64::from(hi) << 32) | u64::from(lo)
}

/// Hardware encoder activates for `codec` on the adapter `luid`, best first.
fn enumerate(codec: VideoCodec, luid: LUID) -> Result<Vec<IMFActivate>, VideoError> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype(codec),
    };
    let mut attributes: Option<IMFAttributes> = None;
    // SAFETY: out-pointer valid for writes.
    unsafe { MFCreateAttributes(&mut attributes, 1) }.ctx("MFCreateAttributes")?;
    let attributes = attributes.ok_or_else(|| VideoError::Other("no attributes".into()))?;
    let luid_bytes = luid_to_bytes(luid);
    // SAFETY: MFT_ENUM_ADAPTER_LUID is a blob holding a LUID.
    unsafe { attributes.SetBlob(&MFT_ENUM_ADAPTER_LUID, &luid_bytes) }
        .ctx("SetBlob(MFT_ENUM_ADAPTER_LUID)")?;

    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: type infos and attributes outlive the call; MF allocates the
    // array with CoTaskMemAlloc and hands us one reference per element.
    unsafe {
        windows::Win32::Media::MediaFoundation::MFTEnum2(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &attributes,
            &mut list,
            &mut count,
        )
    }
    .ctx("MFTEnum2")?;
    let mut activates = Vec::with_capacity(count as usize);
    if !list.is_null() {
        for i in 0..count as usize {
            // SAFETY: `list` has `count` initialised elements; `take` moves
            // each reference out exactly once.
            if let Some(a) = unsafe { (*list.add(i)).take() } {
                activates.push(a);
            }
        }
        // SAFETY: the array came from CoTaskMemAlloc and is now empty.
        unsafe { CoTaskMemFree(Some(list as *const _)) };
    }
    Ok(activates)
}

fn luid_to_bytes(luid: LUID) -> [u8; 8] {
    let mut bytes = [0u8; 8];
    bytes[..4].copy_from_slice(&luid.LowPart.to_le_bytes());
    bytes[4..].copy_from_slice(&luid.HighPart.to_le_bytes());
    bytes
}

fn friendly_name(activate: &IMFActivate) -> String {
    let mut ptr = PWSTR::null();
    let mut len = 0u32;
    // SAFETY: MF allocates the string with CoTaskMemAlloc; freed below.
    if unsafe { activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut ptr, &mut len) }
        .is_err()
        || ptr.is_null()
    {
        return "unknown encoder".into();
    }
    // SAFETY: `ptr` is a valid NUL-terminated wide string of `len` chars.
    let name = unsafe { ptr.to_string() }.unwrap_or_default();
    // SAFETY: allocated by MF with CoTaskMemAlloc.
    unsafe { CoTaskMemFree(Some(ptr.0 as *const _)) };
    name
}

/// Codecs to try for a preference, in order.
fn candidates(pref: CodecPreference) -> &'static [VideoCodec] {
    match pref {
        CodecPreference::Auto => &[VideoCodec::Hevc, VideoCodec::H264],
        CodecPreference::Hevc => &[VideoCodec::Hevc],
        CodecPreference::H264 => &[VideoCodec::H264],
    }
}

fn try_set_codec_value(
    api: &ICodecAPI,
    key: &GUID,
    value: VARIANT,
    context: &'static str,
) -> Result<(), VideoError> {
    // SAFETY: `value` outlives the call; the MFT copies it.
    unsafe { api.SetValue(key, &value) }.ctx(context)
}

fn set_codec_value(api: &ICodecAPI, key: &GUID, value: VARIANT) -> bool {
    try_set_codec_value(api, key, value, "ICodecAPI::SetValue").is_ok()
}

/// Mean and peak bit rate in bit/s for a mean of `kbps`. The one place the
/// peak rule lives: peak-constrained VBR with peak = 1.5 x mean, which lets
/// action scenes burst without the mean drifting from the requested quality.
fn bitrate_bps(kbps: u32) -> (u32, u32) {
    let mean = kbps.saturating_mul(1000);
    (mean, mean.saturating_add(mean / 2))
}

fn get_u32(api: &ICodecAPI, key: &GUID) -> Option<u32> {
    // SAFETY: returns an owned VARIANT.
    let v = unsafe { api.GetValue(key) }.ok()?;
    u32::try_from(&v).ok()
}

fn rate_control_name(mode: u32) -> &'static str {
    // eAVEncCommonRateControlMode values.
    match mode {
        0 => "CBR",
        1 => "peak-constrained VBR",
        2 => "unconstrained VBR",
        3 => "quality-based VBR",
        4 => "low-delay VBR",
        5 => "global VBR",
        6 => "global low-delay VBR",
        _ => "unknown",
    }
}

impl Encoder {
    /// Finds and configures a hardware encoder on `device`'s adapter.
    pub(crate) fn new(
        device: &ID3D11Device,
        luid: LUID,
        adapter_name: &str,
        pref: CodecPreference,
        params: &EncoderParams,
    ) -> Result<Self, VideoError> {
        let mut last_err = None;
        for &codec in candidates(pref) {
            for activate in enumerate(codec, luid)? {
                match Self::from_activate(device, &activate, codec, params) {
                    Ok(encoder) => return Ok(encoder),
                    Err(e) => {
                        // SAFETY: releases the MFT the activate created.
                        let _ = unsafe { activate.ShutdownObject() };
                        last_err = Some(e);
                    }
                }
            }
        }
        Err(last_err.unwrap_or(VideoError::NoEncoder {
            codec: match pref {
                CodecPreference::H264 => "H.264",
                CodecPreference::Hevc => "HEVC",
                CodecPreference::Auto => "HEVC or H.264",
            },
            adapter: adapter_name.to_string(),
        }))
    }

    fn from_activate(
        device: &ID3D11Device,
        activate: &IMFActivate,
        codec: VideoCodec,
        params: &EncoderParams,
    ) -> Result<Self, VideoError> {
        let name = friendly_name(activate);
        // SAFETY: creates the MFT; we own the returned reference.
        let transform: IMFTransform =
            unsafe { activate.ActivateObject() }.ctx("IMFActivate::ActivateObject")?;
        // SAFETY: attribute store owned by the MFT.
        let attrs = unsafe { transform.GetAttributes() }.ctx("IMFTransform::GetAttributes")?;
        // SAFETY: plain attribute reads/writes on a valid store.
        unsafe {
            if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 0 {
                return Err(VideoError::Other(format!(
                    "{name} is not an asynchronous MFT"
                )));
            }
            attrs
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                .ctx("MF_TRANSFORM_ASYNC_UNLOCK")?;
            if attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                return Err(VideoError::Other(format!(
                    "{name} is not Direct3D 11 aware"
                )));
            }
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
        }

        let mut token = 0u32;
        let mut manager = None;
        // SAFETY: out-pointers valid for writes.
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }
            .ctx("MFCreateDXGIDeviceManager")?;
        let manager: IMFDXGIDeviceManager =
            manager.ok_or_else(|| VideoError::Other("no DXGI device manager".into()))?;
        // SAFETY: valid device and the token from the same manager.
        unsafe { manager.ResetDevice(device, token) }.ctx("IMFDXGIDeviceManager::ResetDevice")?;
        // SAFETY: the message takes the manager's IUnknown pointer; the MFT
        // AddRefs it, and we also keep our own reference alive.
        unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize) }
            .ctx("MFT_MESSAGE_SET_D3D_MANAGER")?;

        let (input_id, output_id) = stream_ids(&transform);
        let api: ICodecAPI = transform.cast().ctx("ICodecAPI")?;

        // Some encoders only honour these before the media types are set,
        // others only after; apply them both times.
        apply_codec_settings(&api, params);

        let out_type = media_type(subtype(codec), params)?;
        let profile = match codec {
            VideoCodec::Hevc => eAVEncH265VProfile_Main_420_8.0 as u32,
            _ => eAVEncH264VProfile_High.0 as u32,
        };
        // SAFETY: attribute writes on our own media type.
        unsafe {
            out_type
                .SetUINT32(&MF_MT_AVG_BITRATE, params.bitrate_kbps.saturating_mul(1000))
                .ctx("MF_MT_AVG_BITRATE")?;
            out_type
                .SetUINT32(&MF_MT_MPEG2_PROFILE, profile)
                .ctx("MF_MT_MPEG2_PROFILE")?;
        }
        // SAFETY: valid type; stream id from the MFT.
        unsafe { transform.SetOutputType(output_id, &out_type, 0) }.ctx("SetOutputType")?;

        let in_type = media_type(MFVideoFormat_NV12, params)?;
        // SAFETY: as above.
        unsafe { transform.SetInputType(input_id, &in_type, 0) }.ctx("SetInputType(NV12)")?;

        let rate_control = apply_codec_settings(&api, params);
        let (mean_bps, peak_bps) = bitrate_bps(params.bitrate_kbps);

        // SAFETY: plain getter.
        let info =
            unsafe { transform.GetOutputStreamInfo(output_id) }.ctx("GetOutputStreamInfo")?;
        let provides_samples = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                as u32
            != 0;

        let sequence_header = read_sequence_header(&transform, output_id);

        let events: IMFMediaEventGenerator = transform.cast().ctx("IMFMediaEventGenerator")?;
        // SAFETY: standard streaming start messages.
        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .ctx("MFT_MESSAGE_NOTIFY_BEGIN_STREAMING")?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .ctx("MFT_MESSAGE_NOTIFY_START_OF_STREAM")?;
        }

        Ok(Self {
            activate: activate.clone(),
            transform,
            events,
            _manager: manager,
            api,
            mean_bps,
            peak_bps,
            name,
            codec,
            rate_control,
            sequence_header,
            input_id,
            output_id,
            provides_samples,
            output_size: info.cbSize.max(1 << 20),
            need_input: 0,
            output_sample: None,
        })
    }

    /// Changes the mean bit rate (peak = 1.5 x mean) of the running encoder
    /// without restarting it: the stream stays continuous, so a replay
    /// buffer fed by it keeps its history. The new rate takes effect on
    /// following pictures; the GOP and timestamps are untouched.
    ///
    /// The two `SetValue` calls are ordered so peak >= mean holds at every
    /// step (an encoder may reject or clamp a peak below the mean): raising
    /// sets the peak first, lowering the mean first. Returns the mean in
    /// kbit/s the encoder reports afterwards, or `kbps` if it does not.
    pub(crate) fn set_bitrate(&mut self, kbps: u32) -> Result<u32, VideoError> {
        let (mean, peak) = bitrate_bps(kbps);
        if mean > self.mean_bps {
            self.set_peak(peak)?;
            self.set_mean(mean)?;
        } else {
            self.set_mean(mean)?;
            self.set_peak(peak)?;
        }
        Ok(get_u32(&self.api, &CODECAPI_AVEncCommonMeanBitRate)
            .map_or(kbps, |bps| bps.saturating_add(500) / 1000))
    }

    fn set_mean(&mut self, bps: u32) -> Result<(), VideoError> {
        try_set_codec_value(
            &self.api,
            &CODECAPI_AVEncCommonMeanBitRate,
            VARIANT::from(bps),
            "set mean bit rate",
        )?;
        self.mean_bps = bps;
        Ok(())
    }

    fn set_peak(&mut self, bps: u32) -> Result<(), VideoError> {
        try_set_codec_value(
            &self.api,
            &CODECAPI_AVEncCommonMaxBitRate,
            VARIANT::from(bps),
            "set peak bit rate",
        )?;
        self.peak_bps = bps;
        Ok(())
    }

    /// Drains pending encoder events without waiting. Each encoded access
    /// unit is passed to `on_output(data, sample_time, keyframe)` while the
    /// MF buffer is locked, so it is copied exactly once.
    pub(crate) fn poll(
        &mut self,
        mut on_output: impl FnMut(&[u8], i64, bool),
    ) -> Result<(), VideoError> {
        loop {
            // SAFETY: non-blocking event query on a valid generator.
            let event = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(e) => e,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(win_err("IMFMediaEventGenerator::GetEvent", &e)),
            };
            // SAFETY: plain getters on a valid event.
            let (kind, status) = unsafe { (event.GetType(), event.GetStatus()) };
            let kind = MF_EVENT_TYPE(kind.ctx("IMFMediaEvent::GetType")? as i32);
            let status = status.ctx("IMFMediaEvent::GetStatus")?;
            if status.is_err() || kind == MEError {
                return Err(win_err(
                    "encoder error event",
                    &windows::core::Error::from(status),
                ));
            }
            if kind == METransformNeedInput {
                self.need_input += 1;
            } else if kind == METransformHaveOutput {
                self.process_output(&mut on_output)?;
            }
        }
    }

    /// The encoder has asked for at least one more input sample.
    pub(crate) fn wants_input(&self) -> bool {
        self.need_input > 0
    }

    /// Feeds one frame. Returns `false` if the encoder refused it
    /// (`MF_E_NOTACCEPTING`); the caller counts that as a dropped frame.
    pub(crate) fn submit(&mut self, sample: &IMFSample) -> Result<bool, VideoError> {
        // SAFETY: valid sample; only called after METransformNeedInput.
        match unsafe { self.transform.ProcessInput(self.input_id, sample, 0) } {
            Ok(()) => {
                self.need_input = self.need_input.saturating_sub(1);
                Ok(true)
            }
            Err(e) if e.code() == MF_E_NOTACCEPTING => {
                self.need_input = 0;
                Ok(false)
            }
            Err(e) => Err(win_err("ProcessInput", &e)),
        }
    }

    fn process_output(
        &mut self,
        on_output: &mut impl FnMut(&[u8], i64, bool),
    ) -> Result<(), VideoError> {
        let sample = if self.provides_samples {
            None
        } else {
            Some(self.caller_sample()?)
        };
        let mut buffer = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: self.output_id,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut status = 0u32;
        // SAFETY: one output buffer for our single output stream.
        let result = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status)
        };
        // SAFETY: take ownership of whatever the MFT left in the struct,
        // exactly once each.
        let (sample, _events) = unsafe {
            (
                ManuallyDrop::take(&mut buffer.pSample),
                ManuallyDrop::take(&mut buffer.pEvents),
            )
        };
        match result {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                return self.renegotiate_output();
            }
            Err(e) => return Err(win_err("ProcessOutput", &e)),
        }
        let Some(sample) = sample else {
            return Ok(());
        };
        // SAFETY: plain getters on a valid sample.
        let (time, keyframe) = unsafe {
            (
                sample.GetSampleTime().ctx("GetSampleTime")?,
                sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0,
            )
        };
        // SAFETY: returns a buffer we own.
        let media =
            unsafe { sample.ConvertToContiguousBuffer() }.ctx("ConvertToContiguousBuffer")?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        // SAFETY: Lock gives a pointer valid for `len` bytes until Unlock.
        unsafe { media.Lock(&mut ptr, None, Some(&mut len)) }.ctx("IMFMediaBuffer::Lock")?;
        if !ptr.is_null() && len > 0 {
            // SAFETY: as above; the slice is only used before Unlock.
            let data = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
            on_output(data, time, keyframe);
        }
        // SAFETY: balances the successful Lock.
        let _ = unsafe { media.Unlock() };
        if !self.provides_samples {
            // SAFETY: reset for reuse.
            let _ = unsafe { media.SetCurrentLength(0) };
            self.output_sample = Some(sample);
        }
        Ok(())
    }

    fn caller_sample(&mut self) -> Result<IMFSample, VideoError> {
        if let Some(s) = self.output_sample.take() {
            return Ok(s);
        }
        // SAFETY: plain factory calls; the buffer is attached to the sample.
        unsafe {
            let sample = MFCreateSample().ctx("MFCreateSample")?;
            let buffer = MFCreateMemoryBuffer(self.output_size).ctx("MFCreateMemoryBuffer")?;
            sample.AddBuffer(&buffer).ctx("AddBuffer")?;
            Ok(sample)
        }
    }

    fn renegotiate_output(&mut self) -> Result<(), VideoError> {
        // SAFETY: plain type negotiation on a valid MFT.
        unsafe {
            let t = self
                .transform
                .GetOutputAvailableType(self.output_id, 0)
                .ctx("GetOutputAvailableType")?;
            self.transform
                .SetOutputType(self.output_id, &t, 0)
                .ctx("SetOutputType (stream change)")?;
        }
        if let Some(h) = read_sequence_header(&self.transform, self.output_id) {
            self.sequence_header = Some(h);
        }
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: standard teardown; errors are irrelevant at this point.
        // Flushing releases the queued input samples (back to their pool).
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
            // Releases the activate's own reference to the MFT.
            let _ = self.activate.ShutdownObject();
        }
    }
}

fn stream_ids(transform: &IMFTransform) -> (u32, u32) {
    let mut input = [0u32];
    let mut output = [0u32];
    // SAFETY: arrays sized for one stream each; E_NOTIMPL means ids are 0.
    match unsafe { transform.GetStreamIDs(&mut input, &mut output) } {
        Ok(()) => (input[0], output[0]),
        Err(_) => (0, 0),
    }
}

/// A video media type with the size, rate and BT.709 limited-range colour
/// description shared by the input and output types.
fn media_type(subtype: GUID, p: &EncoderParams) -> Result<IMFMediaType, VideoError> {
    // SAFETY: plain factory call and attribute writes on our own object.
    unsafe {
        let t = MFCreateMediaType().ctx("MFCreateMediaType")?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .ctx("MF_MT_MAJOR_TYPE")?;
        t.SetGUID(&MF_MT_SUBTYPE, &subtype).ctx("MF_MT_SUBTYPE")?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack(p.width, p.height))
            .ctx("MF_MT_FRAME_SIZE")?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack(p.fps, 1))
            .ctx("MF_MT_FRAME_RATE")?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))
            .ctx("MF_MT_PIXEL_ASPECT_RATIO")?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .ctx("MF_MT_INTERLACE_MODE")?;
        t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)
            .ctx("MF_MT_VIDEO_PRIMARIES")?;
        t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)
            .ctx("MF_MT_TRANSFER_FUNCTION")?;
        t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)
            .ctx("MF_MT_YUV_MATRIX")?;
        t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)
            .ctx("MF_MT_VIDEO_NOMINAL_RANGE")?;
        Ok(t)
    }
}

/// GOP = fps (IDR every second), no B-frames, low latency, peak-constrained
/// VBR (falls back to CBR). Returns a description of the rate-control mode
/// the encoder reports afterwards.
fn apply_codec_settings(api: &ICodecAPI, p: &EncoderParams) -> String {
    let (mean, peak) = bitrate_bps(p.bitrate_kbps);
    set_codec_value(api, &CODECAPI_AVEncMPVGOPSize, VARIANT::from(p.fps));
    set_codec_value(
        api,
        &CODECAPI_AVEncMPVDefaultBPictureCount,
        VARIANT::from(0u32),
    );
    set_codec_value(api, &CODECAPI_AVLowLatencyMode, VARIANT::from(true));
    let vbr = set_codec_value(
        api,
        &CODECAPI_AVEncCommonRateControlMode,
        VARIANT::from(eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32),
    );
    if !vbr {
        set_codec_value(
            api,
            &CODECAPI_AVEncCommonRateControlMode,
            VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
        );
    }
    set_codec_value(api, &CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(mean));
    set_codec_value(api, &CODECAPI_AVEncCommonMaxBitRate, VARIANT::from(peak));

    let mode = get_u32(api, &CODECAPI_AVEncCommonRateControlMode);
    let gop = get_u32(api, &CODECAPI_AVEncMPVGOPSize);
    let b = get_u32(api, &CODECAPI_AVEncMPVDefaultBPictureCount);
    let mean_rb = get_u32(api, &CODECAPI_AVEncCommonMeanBitRate);
    let peak_rb = get_u32(api, &CODECAPI_AVEncCommonMaxBitRate);
    let show = |v: Option<u32>| v.map_or_else(|| "?".to_string(), |v| v.to_string());
    format!(
        "{} (mean {} bps, max {} bps, GOP {}, B-frames {})",
        mode.map_or("unreported", rate_control_name),
        show(mean_rb),
        show(peak_rb),
        show(gop),
        show(b),
    )
}

fn read_sequence_header(transform: &IMFTransform, output_id: u32) -> Option<Vec<u8>> {
    // SAFETY: plain getters; the blob is copied into our buffer.
    unsafe {
        let t = transform.GetOutputCurrentType(output_id).ok()?;
        let size = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
        let mut buf = vec![0u8; size as usize];
        t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None)
            .ok()?;
        Some(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::bitrate_bps;

    #[test]
    fn peak_is_one_and_a_half_times_mean() {
        assert_eq!(bitrate_bps(20_000), (20_000_000, 30_000_000));
        assert_eq!(bitrate_bps(4_000), (4_000_000, 6_000_000));
        assert_eq!(bitrate_bps(1), (1_000, 1_500));
    }

    #[test]
    fn peak_never_below_mean_even_when_saturating() {
        for kbps in [1, 999, 20_000, 1_000_000, u32::MAX / 1000, u32::MAX] {
            let (mean, peak) = bitrate_bps(kbps);
            assert!(peak >= mean, "{kbps}");
        }
    }
}
