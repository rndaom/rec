//! WASAPI endpoints: enumeration, shared-mode capture streams and default
//! device change notifications.

use std::sync::Arc;

use rec_core::time::Hns;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE, DEVICE_STATE_ACTIVE, EDataFlow, ERole,
    IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX, eCapture,
    eCommunications, eConsole, eRender,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, STGM_READ};
use windows::core::{HSTRING, PCWSTR, implement};

use crate::sys::{ComGuard, Event};
use crate::{AudioDevice, AudioError, AudioSource, CHANNELS, Context, SAMPLE_RATE};

/// Shared-mode buffer length. Generous so a delayed wake-up never overflows
/// it; it does not add latency (we read whatever is available).
const BUFFER_DURATION: Hns = 2_000_000; // 200 ms

pub(crate) fn enumerator() -> Result<IMMDeviceEnumerator, AudioError> {
    // SAFETY: COM is initialised on the calling thread.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .context("create IMMDeviceEnumerator")
}

/// Data flow and role of the default device a source follows.
fn default_role(source: &AudioSource) -> (EDataFlow, ERole) {
    match source {
        AudioSource::SystemLoopback => (eRender, eConsole),
        AudioSource::Microphone { .. } => (eCapture, eCommunications),
    }
}

/// The device a source currently resolves to.
fn resolve(enumr: &IMMDeviceEnumerator, source: &AudioSource) -> Result<IMMDevice, AudioError> {
    // SAFETY: valid enumerator; the id string outlives the call.
    unsafe {
        match source {
            AudioSource::Microphone {
                device_id: Some(id),
            } => enumr
                .GetDevice(&HSTRING::from(id.as_str()))
                .context("open capture device"),
            _ => {
                let (flow, role) = default_role(source);
                enumr
                    .GetDefaultAudioEndpoint(flow, role)
                    .context("no default audio device")
            }
        }
    }
}

/// Id of the device `source` would open now, if any.
pub(crate) fn current_device_id(
    enumr: &IMMDeviceEnumerator,
    source: &AudioSource,
) -> Option<String> {
    resolve(enumr, source).ok().and_then(|d| device_id(&d).ok())
}

fn device_id(device: &IMMDevice) -> Result<String, AudioError> {
    // SAFETY: GetId returns a CoTaskMem string that we free after copying.
    unsafe {
        let id = device.GetId().context("IMMDevice::GetId")?;
        let s = id.to_string();
        CoTaskMemFree(Some(id.0 as *const _));
        s.map_err(|e| AudioError::Device(format!("device id is not UTF-16: {e}")))
    }
}

fn friendly_name(device: &IMMDevice) -> Option<String> {
    property_string(device, &PKEY_Device_FriendlyName)
}

fn property_string(device: &IMMDevice, key: &PROPERTYKEY) -> Option<String> {
    // SAFETY: the PROPVARIANT is cleared after use and the string returned by
    // PropVariantToStringAlloc is freed after copying.
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let mut value = store.GetValue(key).ok()?;
        let s = PropVariantToStringAlloc(&value).ok().and_then(|p| {
            let s = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const _));
            s
        });
        let _ = PropVariantClear(&mut value);
        s
    }
}

/// Active capture endpoints.
pub fn list_microphones() -> Result<Vec<AudioDevice>, AudioError> {
    let _com = ComGuard::mta()?;
    let enumr = enumerator()?;
    let default = current_device_id(&enumr, &AudioSource::Microphone { device_id: None });
    // SAFETY: valid enumerator and collection; indices are below GetCount.
    unsafe {
        let devices = enumr
            .EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
            .context("EnumAudioEndpoints")?;
        let count = devices
            .GetCount()
            .context("IMMDeviceCollection::GetCount")?;
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let device = devices.Item(i).context("IMMDeviceCollection::Item")?;
            let id = device_id(&device)?;
            out.push(AudioDevice {
                name: friendly_name(&device).unwrap_or_else(|| id.clone()),
                is_default: default.as_deref() == Some(id.as_str()),
                id,
            });
        }
        Ok(out)
    }
}

/// One packet read from a capture stream.
pub(crate) struct Chunk<'a> {
    /// Interleaved 48 kHz stereo i16; `None` when the device flagged silence.
    pub(crate) pcm: Option<&'a [i16]>,
    pub(crate) frames: u32,
    /// QPC time of the first frame, `None` on a timestamp error.
    pub(crate) time: Option<Hns>,
    /// Does not continue the previous chunk.
    pub(crate) discontinuity: bool,
}

/// A started shared-mode capture stream delivering 48 kHz / 16-bit / stereo.
pub(crate) struct Stream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    pub(crate) id: String,
    pub(crate) name: Option<String>,
    next_position: Option<u64>,
}

impl Stream {
    /// Opens and starts a polled capture stream for `source` (no event
    /// handle; see `capture.rs` for why).
    pub(crate) fn open(
        enumr: &IMMDeviceEnumerator,
        source: &AudioSource,
    ) -> Result<Self, AudioError> {
        let device = resolve(enumr, source)?;
        let id = device_id(&device)?;
        let name = friendly_name(&device);
        let block_align = CHANNELS * 2;
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: CHANNELS,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * u32::from(block_align),
            nBlockAlign: block_align,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        // The audio engine converts from the device mix format (any rate,
        // channel layout or sample type) to `format`.
        let mut flags =
            AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        if matches!(source, AudioSource::SystemLoopback) {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        // SAFETY: valid device; `format` outlives Initialize.
        unsafe {
            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .context("activate IAudioClient")?;
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    flags,
                    BUFFER_DURATION,
                    0,
                    &format,
                    None,
                )
                .context("IAudioClient::Initialize")?;
            let capture: IAudioCaptureClient =
                client.GetService().context("IAudioClient::GetService")?;
            client.Start().context("IAudioClient::Start")?;
            Ok(Self {
                client,
                capture,
                id,
                name,
                next_position: None,
            })
        }
    }

    /// Reads every packet that is ready.
    pub(crate) fn read(
        &mut self,
        mut on_chunk: impl FnMut(Chunk<'_>) -> Result<(), AudioError>,
    ) -> Result<(), AudioError> {
        loop {
            // SAFETY: valid capture client.
            let next = unsafe { self.capture.GetNextPacketSize() }.context("GetNextPacketSize")?;
            if next == 0 {
                return Ok(());
            }
            let mut data = std::ptr::null_mut();
            let mut frames = 0;
            let mut flags = 0;
            let mut position = 0;
            let mut qpc = 0;
            // SAFETY: all out pointers are valid; the buffer is released below
            // before the next GetBuffer.
            unsafe {
                self.capture.GetBuffer(
                    &mut data,
                    &mut frames,
                    &mut flags,
                    Some(&mut position),
                    Some(&mut qpc),
                )
            }
            .context("IAudioCaptureClient::GetBuffer")?;
            if frames == 0 {
                // SAFETY: releasing the (empty) buffer we just got.
                unsafe { self.capture.ReleaseBuffer(0) }.context("ReleaseBuffer")?;
                return Ok(());
            }
            let discontinuity = flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0
                || self.next_position.is_some_and(|p| p != position);
            self.next_position = Some(position + u64::from(frames));
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0
                || data.is_null()
                // Never happens (engine buffers are 16-byte aligned), but an
                // unaligned pointer must not become an `&[i16]`.
                || data as usize % std::mem::align_of::<i16>() != 0;
            let time = (flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0 && qpc != 0)
                .then_some(qpc as Hns);
            let pcm = (!silent).then(|| {
                // SAFETY: GetBuffer returned `frames` frames of the format we
                // asked for (2 x i16 each), valid until ReleaseBuffer, and the
                // pointer is aligned (checked above).
                unsafe {
                    std::slice::from_raw_parts(
                        data.cast::<i16>(),
                        frames as usize * usize::from(CHANNELS),
                    )
                }
            });
            let result = on_chunk(Chunk {
                pcm,
                frames,
                time,
                discontinuity,
            });
            // SAFETY: releases exactly the frames returned by GetBuffer; `pcm`
            // is no longer used.
            unsafe { self.capture.ReleaseBuffer(frames) }.context("ReleaseBuffer")?;
            result?;
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: valid client; stopping a stopped or invalidated client only
        // returns an error, which is irrelevant here.
        let _ = unsafe { self.client.Stop() };
    }
}

/// Signals an event on default-device and device-state changes. Runs on an
/// MMDevAPI thread, so it only sets the event; the capture thread decides.
#[implement(IMMNotificationClient)]
struct Notifier {
    event: Arc<Event>,
    flow: EDataFlow,
    role: ERole,
}

impl IMMNotificationClient_Impl for Notifier_Impl {
    fn OnDeviceStateChanged(
        &self,
        _id: &PCWSTR,
        _state: DEVICE_STATE,
    ) -> windows::core::Result<()> {
        self.event.set();
        Ok(())
    }

    fn OnDeviceAdded(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        self.event.set();
        Ok(())
    }

    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        self.event.set();
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        _id: &PCWSTR,
    ) -> windows::core::Result<()> {
        if flow == self.flow && role == self.role {
            self.event.set();
        }
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _id: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

/// Device notifications registered until dropped.
pub(crate) struct Notifications {
    enumr: IMMDeviceEnumerator,
    client: IMMNotificationClient,
}

impl Notifications {
    pub(crate) fn register(
        enumr: &IMMDeviceEnumerator,
        source: &AudioSource,
        event: Arc<Event>,
    ) -> Result<Self, AudioError> {
        let (flow, role) = default_role(source);
        let client: IMMNotificationClient = Notifier { event, flow, role }.into();
        // SAFETY: valid enumerator and client; unregistered in Drop.
        unsafe { enumr.RegisterEndpointNotificationCallback(&client) }
            .context("RegisterEndpointNotificationCallback")?;
        Ok(Self {
            enumr: enumr.clone(),
            client,
        })
    }
}

impl Drop for Notifications {
    fn drop(&mut self) {
        // SAFETY: the client was registered on this enumerator.
        let _ = unsafe {
            self.enumr
                .UnregisterEndpointNotificationCallback(&self.client)
        };
    }
}
