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

use crate::amf::{self, AmfEncoder, AmfParams};
use crate::convert::Converter;
use crate::d3d::{Gpu, VENDOR_AMD};
use crate::encoder::{self, Encoder, EncoderParams};
use crate::geometry::align_up;
use crate::util::Context;
use crate::{CodecPreference, EncoderBackend, VideoError};

pub(crate) struct Backend {
    kind: Kind,
}

// One per pipeline, never moved around: the size difference is irrelevant.
#[allow(clippy::large_enum_variant)]
enum Kind {
    MediaFoundation(Encoder),
    Amf {
        encoder: AmfEncoder,
        /// The pool sample of every slot the encoder may still read, by slot.
        /// Declared after `encoder` so the session goes first on drop.
        in_flight: Vec<Option<IMFSample>>,
    },
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
    /// Media Foundation. `Auto` means AMF on AMD GPUs (NVENC and oneVPL
    /// backends do not exist yet). Notes about backends that were skipped go
    /// to `notes`.
    pub(crate) fn create(
        gpu: &Gpu,
        choice: EncoderBackend,
        codec: CodecPreference,
        params: &EncoderParams,
        notes: &mut Vec<String>,
    ) -> Result<Self, VideoError> {
        let amd = gpu.vendor_id == VENDOR_AMD;
        match choice {
            EncoderBackend::Amf if !amd => notes.push(format!(
                "AMD AMF needs an AMD GPU ({}); using Media Foundation",
                gpu.adapter_name
            )),
            EncoderBackend::Amf | EncoderBackend::Auto if amd => {
                match create_amf(gpu, codec, params) {
                    Ok(encoder) => {
                        return Ok(Self {
                            kind: Kind::Amf {
                                encoder,
                                in_flight: Vec::new(),
                            },
                        });
                    }
                    Err(e) => {
                        notes.push(format!("AMD AMF unavailable ({e}); using Media Foundation"))
                    }
                }
            }
            _ => {}
        }
        let encoder = Encoder::new(&gpu.device, gpu.luid, &gpu.adapter_name, codec, params)?;
        Ok(Self {
            kind: Kind::MediaFoundation(encoder),
        })
    }

    pub(crate) fn name(&self) -> &str {
        match &self.kind {
            Kind::MediaFoundation(e) => &e.name,
            Kind::Amf { encoder, .. } => encoder.name(),
        }
    }

    pub(crate) fn codec(&self) -> VideoCodec {
        match &self.kind {
            Kind::MediaFoundation(e) => e.codec,
            Kind::Amf { encoder, .. } => encoder.codec(),
        }
    }

    /// Size of the encoded frames; can exceed the requested size where the
    /// hardware needs alignment (the picture is then centred in it).
    pub(crate) fn coded_size(&self) -> (u32, u32) {
        match &self.kind {
            Kind::MediaFoundation(e) => (e.width, e.height),
            Kind::Amf { encoder, .. } => encoder.coded_size(),
        }
    }

    pub(crate) fn rate_control(&self) -> &str {
        match &self.kind {
            Kind::MediaFoundation(e) => &e.rate_control,
            Kind::Amf { encoder, .. } => encoder.rate_control(),
        }
    }

    /// Parameter sets / sequence header the encoder announced up front.
    pub(crate) fn sequence_header(&self) -> Option<&[u8]> {
        match &self.kind {
            Kind::MediaFoundation(e) => e.sequence_header.as_deref(),
            // AMF announces nothing up front; every keyframe carries them.
            Kind::Amf { .. } => None,
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
            Kind::Amf { encoder, in_flight } => encoder.poll(on_output, |slot| {
                // Dropping the sample returns the surface to the pool.
                if let Some(sample) = in_flight.get_mut(slot) {
                    *sample = None;
                }
            }),
        }
    }

    /// Whether a picture submitted now would be taken.
    pub(crate) fn wants_input(&self) -> bool {
        match &self.kind {
            Kind::MediaFoundation(e) => e.wants_input(),
            // AMF queues 16 frames and says so in `submit` when full.
            Kind::Amf { .. } => true,
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
        match &mut self.kind {
            Kind::MediaFoundation(e) => {
                let sample = timed_sample(converter, slot, time, duration)?;
                // Dropping our reference afterwards returns the surface to
                // the pool as soon as the encoder is done with it.
                e.submit(&sample)
            }
            Kind::Amf { encoder, in_flight } => {
                // Taking the sample marks the slot busy until it is dropped.
                let sample = converter.take_sample(slot)?;
                let taken = encoder.submit(converter.texture(slot), slot, time, duration)?;
                if taken {
                    if in_flight.len() <= slot {
                        in_flight.resize(slot + 1, None);
                    }
                    in_flight[slot] = Some(sample);
                }
                Ok(taken)
            }
        }
    }

    pub(crate) fn set_bitrate(&mut self, kbps: u32) -> Result<u32, VideoError> {
        match &mut self.kind {
            Kind::MediaFoundation(e) => e.set_bitrate(kbps),
            Kind::Amf { encoder, .. } => encoder.set_bitrate(kbps),
        }
    }
}

/// An AMF encoder for the first of `pref`'s codecs that works, at `params`'
/// size padded to the codec's alignment (the pipeline centres the picture
/// in it, as for Media Foundation).
fn create_amf(
    gpu: &Gpu,
    pref: CodecPreference,
    params: &EncoderParams,
) -> Result<AmfEncoder, VideoError> {
    let mut last = None;
    for &codec in encoder::candidates(pref) {
        if encoder::profile(codec, params.color).is_none() {
            continue;
        }
        let created = amf::size_alignment(&gpu.device, codec).and_then(|(aw, ah)| {
            AmfEncoder::new(
                &gpu.device,
                &AmfParams {
                    codec,
                    width: align_up(params.width, aw),
                    height: align_up(params.height, ah),
                    fps: params.fps,
                    bitrate_kbps: params.bitrate_kbps,
                    color: params.color,
                },
            )
        });
        match created {
            Ok(encoder) => return Ok(encoder),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or(VideoError::NoEncoder {
        codec: encoder::preference_name(pref),
        adapter: gpu.adapter_name.clone(),
    }))
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
