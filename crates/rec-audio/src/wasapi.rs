//! WASAPI endpoints: enumeration, shared-mode capture streams and default
//! device change notifications.

use std::sync::Arc;

use rec_core::time::{Hns, millis};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE, DEVICE_STATE_ACTIVE, EDataFlow, ERole,
    IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX, eCapture,
    eCommunications, eConsole, eRender,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, STGM_READ};
use windows::core::{HSTRING, PCWSTR, implement};

use crate::process::{self, Followed};
use crate::sys::{ComGuard, Event};
use crate::timeline::samples_to_hns;
use crate::{AudioDevice, AudioError, AudioSource, CHANNELS, Context, SAMPLE_RATE};

/// Shared-mode buffer length. Generous so a delayed wake-up never overflows
/// it; it does not add latency (we read whatever is available).
const BUFFER_DURATION: Hns = 2_000_000; // 200 ms

/// Process loopback reports no device position, so a timestamp this far from
/// where the previous packet ended marks a discontinuity (the stream jumps by
/// 15-50 ms when the app starts or stops playing; packet jitter is far below
/// this).
const TIMESTAMP_JUMP: Hns = millis(3);

/// Process loopback stamps the same sound this much later than endpoint
/// loopback does (measured by cross-correlating both captures of one app:
/// 19.6-21.7 ms over four runs, Windows 11 26200, USB headset). Endpoint
/// loopback stamps are presentation times, when the sound is heard, which is
/// what clips need, so process loopback timestamps are moved back by this.
const PROCESS_LOOPBACK_LATE: Hns = millis(21);

pub(crate) fn enumerator() -> Result<IMMDeviceEnumerator, AudioError> {
    // SAFETY: COM is initialised on the calling thread.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .context("create IMMDeviceEnumerator")
}

/// Data flow and role of the default device a source follows. Process
/// loopback follows the default render device too: it is reopened when that
/// changes.
fn default_role(source: &AudioSource) -> (EDataFlow, ERole) {
    match source {
        AudioSource::SystemLoopback | AudioSource::Process { .. } => (eRender, eConsole),
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
    // SAFETY: the PROPVARIANT is cleared when dropped (windows-rs implements
    // Drop with PropVariantClear) and the string returned by
    // PropVariantToStringAlloc is freed after copying.
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let value = store.GetValue(key).ok()?;
        PropVariantToStringAlloc(&value).ok().and_then(|p| {
            let s = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const _));
            s
        })
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Capture,
    Loopback,
    /// Process loopback: no device position, event-driven.
    Process,
}

/// A started shared-mode capture stream delivering 48 kHz / 16-bit / stereo.
pub(crate) struct Stream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    /// The endpoint captured; for process loopback, the default render
    /// endpoint when the stream was opened.
    pub(crate) id: String,
    pub(crate) name: Option<String>,
    /// Signalled when data is ready (process loopback only; endpoint streams
    /// are polled, see `capture.rs`).
    pub(crate) event: Option<Event>,
    next_position: Option<u64>,
    /// Process loopback: continuity is judged from timestamps.
    next_time: Option<Hns>,
    position_reliable: bool,
}

impl Stream {
    /// Opens and starts a capture stream for `source`. Process loopback needs
    /// the process to follow (`followed`) and is event-driven; endpoint
    /// streams are polled.
    pub(crate) fn open(
        enumr: &IMMDeviceEnumerator,
        source: &AudioSource,
        followed: Option<&Followed>,
    ) -> Result<Self, AudioError> {
        if let AudioSource::Process { exclude, .. } = source {
            let followed =
                followed.ok_or_else(|| AudioError::Device("the app is not running".into()))?;
            let client = process::activate(followed.pid, *exclude)?;
            // Only used to notice default-device changes.
            let id = current_device_id(enumr, source).unwrap_or_default();
            let name = if *exclude {
                format!("everything except {}", followed.describe())
            } else {
                followed.describe()
            };
            return Self::start(client, id, Some(name), Kind::Process);
        }
        let device = resolve(enumr, source)?;
        let id = device_id(&device)?;
        let name = friendly_name(&device);
        // SAFETY: valid device.
        let client: IAudioClient =
            unsafe { device.Activate(CLSCTX_ALL, None) }.context("activate IAudioClient")?;
        let kind = match source {
            AudioSource::SystemLoopback => Kind::Loopback,
            _ => Kind::Capture,
        };
        Self::start(client, id, name, kind)
    }

    fn start(
        client: IAudioClient,
        id: String,
        name: Option<String>,
        kind: Kind,
    ) -> Result<Self, AudioError> {
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
        match kind {
            Kind::Capture => {}
            Kind::Loopback => flags |= AUDCLNT_STREAMFLAGS_LOOPBACK,
            Kind::Process => {
                flags |= AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
            }
        }
        let event = match kind {
            Kind::Process => Some(Event::new()?),
            _ => None,
        };
        // SAFETY: valid client; `format` outlives Initialize; the event
        // handle outlives the client (both are owned by the new Stream, and
        // the client is stopped in Drop before the event closes).
        unsafe {
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
            if let Some(event) = &event {
                client
                    .SetEventHandle(event.handle())
                    .context("IAudioClient::SetEventHandle")?;
            }
            let capture: IAudioCaptureClient =
                client.GetService().context("IAudioClient::GetService")?;
            client.Start().context("IAudioClient::Start")?;
            Ok(Self {
                client,
                capture,
                id,
                name,
                event,
                next_position: None,
                next_time: None,
                position_reliable: kind != Kind::Process,
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
            let late = if self.position_reliable {
                0
            } else {
                PROCESS_LOOPBACK_LATE
            };
            let time = (flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0 && qpc != 0)
                .then_some(qpc as Hns - late);
            let jumped = if self.position_reliable {
                self.next_position.is_some_and(|p| p != position)
            } else {
                matches!((self.next_time, time), (Some(e), Some(t)) if (t - e).abs() > TIMESTAMP_JUMP)
            };
            let discontinuity =
                flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 || jumped;
            self.next_position = Some(position + u64::from(frames));
            self.next_time = time.map(|t| t + samples_to_hns(frames.into()));
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0
                || data.is_null()
                // Never happens (engine buffers are 16-byte aligned), but an
                // unaligned pointer must not become an `&[i16]`.
                || data as usize % std::mem::align_of::<i16>() != 0;
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
    /// Default devices whose change matters.
    roles: Vec<(EDataFlow, ERole)>,
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
        if self.roles.contains(&(flow, role)) {
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
    /// Signals `event` when a device the `sources` may use changes.
    pub(crate) fn register<'a>(
        enumr: &IMMDeviceEnumerator,
        sources: impl IntoIterator<Item = &'a AudioSource>,
        event: Arc<Event>,
    ) -> Result<Self, AudioError> {
        let mut roles: Vec<(EDataFlow, ERole)> = Vec::new();
        for role in sources.into_iter().map(default_role) {
            if !roles.contains(&role) {
                roles.push(role);
            }
        }
        let client: IMMNotificationClient = Notifier { event, roles }.into();
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
