//! The encoder input surfaces (NV12 or P010) in flight between the converter
//! and the encoder.
//!
//! Each surface is wrapped once in an `IMFTrackedSample` around an
//! `MFCreateDXGISurfaceBuffer` (zero copy). When the last reference to a
//! sample goes away, the tracked sample calls back and the slot becomes free
//! again, so a surface is never overwritten while the encoder still reads
//! it. The Media Foundation encoder holds the sample itself; for a native
//! encoder the pipeline holds it until the encoder reports the surface done.

use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::E_NOTIMPL;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::{
    IMFAsyncCallback, IMFAsyncCallback_Impl, IMFAsyncResult, IMFSample, IMFTrackedSample,
    MFCreateDXGISurfaceBuffer, MFCreateTrackedSample,
};
use windows::core::{Interface, Ref, implement};

use crate::VideoError;
use crate::util::Context;

/// Surfaces in flight. Four cover the Media Foundation encoders; native
/// encoders keep a frame or two more in their queue.
pub(crate) const POOL_SIZE: usize = 6;

/// An `IMFSample` that crosses to the MF thread that releases it.
pub(crate) struct SendSample(pub IMFSample);
// SAFETY: Media Foundation samples are free-threaded objects; ownership is
// handed over whole (no shared aliasing), guarded by the `FreeList` mutex.
unsafe impl Send for SendSample {}

/// Samples whose surface is free, indexed by slot.
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

/// Tracked-sample callback: the last reference to the sample was dropped.
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
    callback: IMFAsyncCallback,
}

pub(crate) struct Pool {
    slots: Vec<Slot>,
    free: Arc<FreeList>,
    pub format: DXGI_FORMAT,
    pub width: u32,
    pub height: u32,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("format", &self.format)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl Pool {
    pub(crate) fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> Result<Self, VideoError> {
        let free = Arc::new(FreeList::default());
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut slots = Vec::with_capacity(POOL_SIZE);
        let mut samples = Vec::with_capacity(POOL_SIZE);
        for slot in 0..POOL_SIZE {
            let mut texture = None;
            // SAFETY: valid descriptor; out-pointer valid for writes.
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
                .ctx("CreateTexture2D(encoder input)")?;
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
            slots.push(Slot { texture, callback });
        }
        *free
            .0
            .lock()
            .map_err(|_| VideoError::Other("lock poisoned".into()))? =
            samples.into_iter().map(Some).collect();
        Ok(Self {
            slots,
            free,
            format,
            width,
            height,
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    /// True if `slot`'s surface is not in use.
    pub(crate) fn is_free(&self, slot: usize) -> bool {
        self.free.is_free(slot)
    }

    /// A free slot, preferring one other than `avoid` (the last submitted).
    pub(crate) fn free_slot(&self, avoid: Option<usize>) -> Option<usize> {
        self.free
            .any_free(avoid)
            .or_else(|| self.free.any_free(None))
    }

    pub(crate) fn texture(&self, slot: usize) -> &ID3D11Texture2D {
        &self.slots[slot].texture
    }

    /// Takes `slot`'s sample for submission and arms its return callback:
    /// the slot is free again once every reference to the sample is gone.
    pub(crate) fn take_sample(&self, slot: usize) -> Result<IMFSample, VideoError> {
        let sample = self
            .free
            .take(slot)
            .ok_or_else(|| VideoError::Other(format!("encoder input slot {slot} is not free")))?;
        let tracked: IMFTrackedSample = sample.cast().ctx("IMFTrackedSample")?;
        // SAFETY: valid callback; the sample calls it once when released.
        unsafe { tracked.SetAllocator(&self.slots[slot].callback, None) }
            .ctx("IMFTrackedSample::SetAllocator")?;
        Ok(sample)
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // Break the sample → callback → free list → sample cycle.
        self.free.clear();
    }
}
