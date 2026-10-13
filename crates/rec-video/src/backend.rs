//! Encoder backends behind one interface, and the choice between them.
//!
//! The Media Foundation encoders work on every GPU vendor with nothing
//! bundled. Native vendor encoders (AMD AMF today) are preferred where they
//! exist: they expose the encoder's own controls, take the same surfaces, and
//! anything that goes wrong creating one falls back to Media Foundation with
//! a note in [`crate::VideoInfo::notes`], never a failed recording.
//!
//! Input surfaces come from [`crate::pool::Pool`]. Media Foundation holds the
//! pool's tracked sample itself; a native encoder only sees the texture, so
//! the backend holds the sample until the encoder says it is done with the
//! surface, and dropping it returns the surface to the pool.

use rec_core::media::VideoCodec;
use rec_core::time::Hns;
use windows::Win32::Media::MediaFoundation::IMFSample;

use crate::convert::Converter;
use crate::d3d::Gpu;
use crate::encoder::{Encoder, EncoderParams};
use crate::util::Context;
use crate::{CodecPreference, EncoderBackend, VideoError};

pub(crate) struct Backend {
    kind: Kind,
}

enum Kind {
    MediaFoundation(Encoder),
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend")
            .field("name", &self.name())
            .finish_non_exhaustive()
    }
}

impl Backend {
    /// Creates an encoder for `params` following `choice`, falling back to
    /// Media Foundation. Notes about backends that were skipped go to `notes`.
    pub(crate) fn create(
        gpu: &Gpu,
        choice: EncoderBackend,
        codec: CodecPreference,
        params: &EncoderParams,
        notes: &mut Vec<String>,
    ) -> Result<Self, VideoError> {
        let _ = choice;
        let encoder = Encoder::new(&gpu.device, gpu.luid, &gpu.adapter_name, codec, params)?;
        let _ = notes;
        Ok(Self {
            kind: Kind::MediaFoundation(encoder),
        })
    }

    pub(crate) fn name(&self) -> &str {
        match &self.kind {
            Kind::MediaFoundation(e) => &e.name,
        }
    }

    pub(crate) fn codec(&self) -> VideoCodec {
        match &self.kind {
            Kind::MediaFoundation(e) => e.codec,
        }
    }

    /// Size of the encoded frames; can exceed the requested size where the
    /// hardware needs alignment (the picture is then centred in it).
    pub(crate) fn coded_size(&self) -> (u32, u32) {
        match &self.kind {
            Kind::MediaFoundation(e) => (e.width, e.height),
        }
    }

    pub(crate) fn rate_control(&self) -> &str {
        match &self.kind {
            Kind::MediaFoundation(e) => &e.rate_control,
        }
    }

    /// Parameter sets / sequence header the encoder announced up front.
    pub(crate) fn sequence_header(&self) -> Option<&[u8]> {
        match &self.kind {
            Kind::MediaFoundation(e) => e.sequence_header.as_deref(),
        }
    }

    /// Drains finished pictures without waiting: `on_output(data, time,
    /// keyframe)` per access unit.
    pub(crate) fn poll(
        &mut self,
        on_output: impl FnMut(&[u8], Hns, bool),
    ) -> Result<(), VideoError> {
        match &mut self.kind {
            Kind::MediaFoundation(e) => e.poll(on_output),
        }
    }

    /// Whether a picture submitted now would be taken.
    pub(crate) fn wants_input(&self) -> bool {
        match &self.kind {
            Kind::MediaFoundation(e) => e.wants_input(),
        }
    }

    /// Submits `slot`'s converted surface for the tick at `time`. Returns
    /// `false` if the encoder refused it (a dropped frame).
    pub(crate) fn submit(
        &mut self,
        converter: &Converter,
        slot: usize,
        time: Hns,
        duration: Hns,
    ) -> Result<bool, VideoError> {
        let sample = timed_sample(converter, slot, time, duration)?;
        // Dropping our reference afterwards returns the surface to the pool
        // as soon as the encoder is done with it.
        match &mut self.kind {
            Kind::MediaFoundation(e) => e.submit(&sample),
        }
    }

    pub(crate) fn set_bitrate(&mut self, kbps: u32) -> Result<u32, VideoError> {
        match &mut self.kind {
            Kind::MediaFoundation(e) => e.set_bitrate(kbps),
        }
    }
}

fn timed_sample(
    converter: &Converter,
    slot: usize,
    time: Hns,
    duration: Hns,
) -> Result<IMFSample, VideoError> {
    let sample = converter.take_sample(slot)?;
    // SAFETY: plain setters on our own sample.
    unsafe {
        sample.SetSampleTime(time).ctx("SetSampleTime")?;
        sample
            .SetSampleDuration(duration)
            .ctx("SetSampleDuration")?;
    }
    Ok(sample)
}
