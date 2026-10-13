//! Media Foundation AAC encoder (CLSID_AACMFTEncoder) wrapper:
//! 48 kHz / 16-bit / stereo PCM in, raw AAC-LC access units out.

use std::mem::ManuallyDrop;

use rec_core::media::{AudioCodec, AudioFormat};
use windows::Win32::Media::MediaFoundation::{
    AACMFTEncoder, IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform, MF_E_NOTACCEPTING,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION,
    MF_MT_AAC_PAYLOAD_TYPE, MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE,
    MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_MT_USER_DATA, MFAudioFormat_AAC, MFAudioFormat_PCM,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Audio,
    MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};

use crate::aac::{self, AscInfo};
use crate::sys::ref_count;
use crate::timeline::samples_to_hns;
use crate::{AudioError, CHANNELS, Context, SAMPLE_RATE};

const BYTES_PER_FRAME: u32 = 2 * CHANNELS as u32;
/// Input buffer size: 100 ms. Larger pushes are split.
const INPUT_FRAMES: u32 = SAMPLE_RATE / 10;
/// AAC-LC profile 2, level 2 (the encoder default; `0x29`).
const AAC_PROFILE_LEVEL: u32 = 0x29;

pub(crate) struct AacEncoder {
    mft: IMFTransform,
    input: Buffered,
    /// Our output sample, unless the MFT allocates its own.
    output: Option<Buffered>,
    /// Samples fed so far (for input sample times).
    fed: i64,
    format: AudioFormat,
    bitrate_kbps: u32,
}

/// A sample with one memory buffer that we reuse across calls.
struct Buffered {
    sample: IMFSample,
    buffer: IMFMediaBuffer,
}

impl Buffered {
    fn new(bytes: u32) -> Result<Self, AudioError> {
        // SAFETY: plain MF object creation; Media Foundation is started by
        // the caller (documented crate requirement).
        unsafe {
            let sample = MFCreateSample().context("MFCreateSample")?;
            let buffer = MFCreateMemoryBuffer(bytes).context("MFCreateMemoryBuffer")?;
            sample.AddBuffer(&buffer).context("IMFSample::AddBuffer")?;
            Ok(Self { sample, buffer })
        }
    }

    /// True if nobody but us references the sample and its buffer, so the
    /// memory may be overwritten.
    fn is_exclusive(&self) -> bool {
        // We hold one reference to the sample; the buffer is held by us and by
        // the sample.
        ref_count(&self.sample) == 1 && ref_count(&self.buffer) == 2
    }
}

impl AacEncoder {
    pub(crate) fn new(bitrate_kbps: u32) -> Result<Self, AudioError> {
        let bitrate_kbps = aac::nearest_bitrate_kbps(bitrate_kbps);
        // SAFETY: COM is initialised on this thread by the caller; all
        // pointers passed below are valid for the duration of each call.
        unsafe {
            let mft: IMFTransform = CoCreateInstance(&AACMFTEncoder, None, CLSCTX_INPROC_SERVER)
                .context("create Media Foundation AAC encoder")?;

            let input = MFCreateMediaType().context("MFCreateMediaType")?;
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                .context("input type")?;
            input
                .SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)
                .context("input type")?;
            input
                .SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)
                .context("input type")?;
            input
                .SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)
                .context("input type")?;
            input
                .SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS.into())
                .context("input type")?;
            input
                .SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, BYTES_PER_FRAME)
                .context("input type")?;
            input
                .SetUINT32(
                    &MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
                    SAMPLE_RATE * BYTES_PER_FRAME,
                )
                .context("input type")?;

            let output = MFCreateMediaType().context("MFCreateMediaType")?;
            output
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                .context("output type")?;
            output
                .SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)
                .context("output type")?;
            output
                .SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)
                .context("output type")?;
            output
                .SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)
                .context("output type")?;
            output
                .SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS.into())
                .context("output type")?;
            output
                .SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bitrate_kbps * 1000 / 8)
                .context("output type")?;
            // 0 = raw access units (no ADTS / LOAS framing).
            output
                .SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)
                .context("output type")?;
            output
                .SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, AAC_PROFILE_LEVEL)
                .context("output type")?;

            // The AAC encoder wants the input type first.
            mft.SetInputType(0, &input, 0)
                .context("AAC encoder SetInputType")?;
            mft.SetOutputType(0, &output, 0)
                .context("AAC encoder SetOutputType")?;

            let current = mft
                .GetOutputCurrentType(0)
                .context("AAC encoder GetOutputCurrentType")?;
            let codec_config = audio_specific_config(&current)?;

            let info = mft
                .GetOutputStreamInfo(0)
                .context("AAC encoder GetOutputStreamInfo")?;
            let provides = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            let output = if provides {
                None
            } else {
                // One access unit is at most 6144 bits per channel.
                Some(Buffered::new(info.cbSize.max(768 * u32::from(CHANNELS)))?)
            };

            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("AAC encoder begin streaming")?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("AAC encoder start of stream")?;

            Ok(Self {
                mft,
                input: Buffered::new(INPUT_FRAMES * BYTES_PER_FRAME)?,
                output,
                fed: 0,
                format: AudioFormat {
                    codec: AudioCodec::Aac,
                    sample_rate: SAMPLE_RATE,
                    channels: CHANNELS,
                    codec_config,
                },
                bitrate_kbps,
            })
        }
    }

    pub(crate) fn format(&self) -> &AudioFormat {
        &self.format
    }

    pub(crate) fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps
    }

    /// Encodes interleaved stereo frames; `emit` receives each finished access
    /// unit in order.
    pub(crate) fn push_pcm(
        &mut self,
        pcm: &[i16],
        emit: &mut dyn FnMut(&[u8]),
    ) -> Result<(), AudioError> {
        for chunk in pcm.chunks(INPUT_FRAMES as usize * usize::from(CHANNELS)) {
            self.push_chunk(Some(chunk), chunk.len() / usize::from(CHANNELS), emit)?;
        }
        Ok(())
    }

    /// Encodes `frames` frames of silence.
    pub(crate) fn push_silence(
        &mut self,
        mut frames: u64,
        emit: &mut dyn FnMut(&[u8]),
    ) -> Result<(), AudioError> {
        while frames > 0 {
            let n = frames.min(INPUT_FRAMES.into());
            self.push_chunk(None, n as usize, emit)?;
            frames -= n;
        }
        Ok(())
    }

    /// Flushes the encoder's pending frames (end of stream).
    pub(crate) fn drain(&mut self, emit: &mut dyn FnMut(&[u8])) -> Result<(), AudioError> {
        // SAFETY: valid MFT; the message takes no pointer parameter.
        unsafe { self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }
            .context("AAC encoder drain")?;
        self.pull_output(emit)
    }

    fn push_chunk(
        &mut self,
        pcm: Option<&[i16]>,
        frames: usize,
        emit: &mut dyn FnMut(&[u8]),
    ) -> Result<(), AudioError> {
        if !self.input.is_exclusive() {
            // The encoder still references the last input; never overwrite it.
            self.input = Buffered::new(INPUT_FRAMES * BYTES_PER_FRAME)?;
        }
        let bytes = frames * BYTES_PER_FRAME as usize;
        // SAFETY: the buffer is exclusively ours (checked above) and at least
        // INPUT_FRAMES frames long, and `frames <= INPUT_FRAMES`. Lock returns
        // a pointer valid for `max_len` bytes until Unlock.
        unsafe {
            let mut ptr = std::ptr::null_mut();
            let mut max_len = 0;
            self.input
                .buffer
                .Lock(&mut ptr, Some(&mut max_len), None)
                .context("IMFMediaBuffer::Lock")?;
            debug_assert!(bytes <= max_len as usize);
            match pcm {
                Some(pcm) => {
                    std::ptr::copy_nonoverlapping(pcm.as_ptr().cast::<u8>(), ptr, bytes);
                }
                None => std::ptr::write_bytes(ptr, 0, bytes),
            }
            self.input
                .buffer
                .Unlock()
                .context("IMFMediaBuffer::Unlock")?;
            self.input
                .buffer
                .SetCurrentLength(bytes as u32)
                .context("IMFMediaBuffer::SetCurrentLength")?;
            let sample = &self.input.sample;
            sample
                .SetSampleTime(samples_to_hns(self.fed))
                .context("IMFSample::SetSampleTime")?;
            sample
                .SetSampleDuration(samples_to_hns(frames as i64))
                .context("IMFSample::SetSampleDuration")?;
        }
        self.fed += frames as i64;

        // SAFETY: valid MFT and sample.
        let mut result = unsafe { self.mft.ProcessInput(0, &self.input.sample, 0) };
        if matches!(&result, Err(e) if e.code() == MF_E_NOTACCEPTING) {
            self.pull_output(emit)?;
            // SAFETY: as above.
            result = unsafe { self.mft.ProcessInput(0, &self.input.sample, 0) };
        }
        result.context("AAC encoder ProcessInput")?;
        self.pull_output(emit)
    }

    fn pull_output(&mut self, emit: &mut dyn FnMut(&[u8])) -> Result<(), AudioError> {
        loop {
            if let Some(out) = &self.output {
                // SAFETY: valid buffer; resetting the length is always allowed.
                unsafe { out.buffer.SetCurrentLength(0) }.context("SetCurrentLength")?;
            }
            let mut data = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(self.output.as_ref().map(|o| o.sample.clone())),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };
            let mut status = 0;
            // SAFETY: `data` is a valid single-element array; the MFT writes
            // into our sample or replaces `pSample` with its own (owned
            // reference), and may set `pEvents`; both are released below.
            let result = unsafe {
                self.mft
                    .ProcessOutput(0, std::slice::from_mut(&mut data), &mut status)
            };
            let sample = ManuallyDrop::into_inner(data.pSample);
            drop(ManuallyDrop::into_inner(data.pEvents));
            match result {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) => {
                    return Err(AudioError::Windows {
                        context: "AAC encoder ProcessOutput",
                        source: e,
                    });
                }
            }
            let Some(sample) = sample else { continue };
            // SAFETY: the sample holds a contiguous buffer after ProcessOutput
            // (ours has exactly one); Lock gives a pointer valid for `len`
            // bytes until Unlock.
            unsafe {
                let buffer = match &self.output {
                    Some(out) => out.buffer.clone(),
                    None => sample
                        .ConvertToContiguousBuffer()
                        .context("ConvertToContiguousBuffer")?,
                };
                let mut ptr = std::ptr::null_mut();
                let mut len = 0;
                buffer
                    .Lock(&mut ptr, None, Some(&mut len))
                    .context("IMFMediaBuffer::Lock")?;
                if len > 0 {
                    emit(std::slice::from_raw_parts(ptr, len as usize));
                }
                buffer.Unlock().context("IMFMediaBuffer::Unlock")?;
            }
        }
    }
}

/// Reads the AudioSpecificConfig from `MF_MT_USER_DATA` and checks it.
fn audio_specific_config(media_type: &IMFMediaType) -> Result<Vec<u8>, AudioError> {
    let expected = AscInfo {
        object_type: aac::OBJECT_TYPE_AAC_LC,
        sample_rate: SAMPLE_RATE,
        channels: CHANNELS as u8,
    };
    // SAFETY: valid media type; GetBlob writes at most `buf.len()` bytes.
    let user_data = unsafe {
        let size = media_type
            .GetBlobSize(&MF_MT_USER_DATA)
            .context("AAC MF_MT_USER_DATA")?;
        let mut buf = vec![0u8; size as usize];
        media_type
            .GetBlob(&MF_MT_USER_DATA, &mut buf, None)
            .context("AAC MF_MT_USER_DATA")?;
        buf
    };
    aac::asc_from_user_data(&user_data, expected).ok_or_else(|| {
        AudioError::Encoder(format!(
            "unexpected AudioSpecificConfig in MF_MT_USER_DATA: {user_data:02x?}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Once;

    use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_LITE, MFStartup};

    use super::*;
    use crate::sys::ComGuard;

    /// Starts Media Foundation and creates an encoder, or returns `None` when
    /// this Windows has no Media Foundation (N editions, Server Core).
    fn encoder(kbps: u32) -> Option<(ComGuard, AacEncoder)> {
        static STARTUP: Once = Once::new();
        // SAFETY: process-wide MF startup, done once and never shut down.
        STARTUP.call_once(|| unsafe {
            let _ = MFStartup(MF_VERSION, MFSTARTUP_LITE);
        });
        let com = ComGuard::mta().unwrap();
        match AacEncoder::new(kbps) {
            Ok(e) => Some((com, e)),
            Err(e) => {
                eprintln!("skipping: Media Foundation AAC encoder unavailable: {e}");
                None
            }
        }
    }

    fn encode(enc: &mut AacEncoder, pcm: &[i16]) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();
        // WASAPI-sized pushes (10 ms).
        for chunk in pcm.chunks(960) {
            enc.push_pcm(chunk, &mut |p| packets.push(p.to_vec()))
                .unwrap();
        }
        enc.drain(&mut |p| packets.push(p.to_vec())).unwrap();
        packets
    }

    #[test]
    fn encodes_aac_lc_48k_stereo() {
        let Some((_com, mut enc)) = encoder(150) else {
            return;
        };
        assert_eq!(enc.bitrate_kbps(), 160);
        let f = enc.format();
        assert_eq!((f.sample_rate, f.channels), (48_000, 2));
        assert_eq!(f.codec_config, [0x11, 0x90]);

        // One second of a 1 kHz tone, then a second of silence fed as such.
        let pcm: Vec<i16> = (0..48_000)
            .flat_map(|i| {
                let v =
                    ((i as f64 * 1000.0 / 48_000.0 * std::f64::consts::TAU).sin() * 8000.0) as i16;
                [v, v]
            })
            .collect();
        let mut packets = 0u64;
        let mut bytes = 0;
        for chunk in pcm.chunks(960) {
            enc.push_pcm(chunk, &mut |p| {
                packets += 1;
                bytes += p.len();
            })
            .unwrap();
        }
        // The encoder copies its input, so the input sample is reused, not
        // reallocated per push.
        assert!(enc.input.is_exclusive());
        enc.push_silence(48_000, &mut |_| packets += 1).unwrap();
        enc.drain(&mut |_| packets += 1).unwrap();
        // 96 000 samples plus the encoder delay, in 1024-sample packets.
        let expected = (96_000 + crate::ENCODER_DELAY_SAMPLES as u64).div_ceil(1024);
        assert!(
            packets.abs_diff(expected) <= 1,
            "{packets} packets, expected about {expected}"
        );
        // ~160 kbit/s for the tone second.
        assert!((15_000..25_000).contains(&bytes), "{bytes} bytes");
    }

    /// Encoder speed (run with `--release`): CPU cost of encoding 60 s of a
    /// noisy tone and of 60 s of silence, in 10 ms pushes.
    #[test]
    #[ignore = "benchmark"]
    fn encoder_speed() {
        let Some((_com, mut enc)) = encoder(160) else {
            return;
        };
        let mut seed = 1u32;
        let pcm: Vec<i16> = (0..48_000 * 60)
            .flat_map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = f64::from((seed >> 16) as i16) / 8.0;
                let tone = (f64::from(i) * 440.0 / 48_000.0 * std::f64::consts::TAU).sin() * 6000.0;
                let v = (tone + noise) as i16;
                [v, v / 2]
            })
            .collect();
        let t = std::time::Instant::now();
        let packets = encode(&mut enc, &pcm).len();
        let audio = t.elapsed();
        let t = std::time::Instant::now();
        enc.push_silence(48_000 * 60, &mut |_| {}).unwrap();
        let silence = t.elapsed();
        println!(
            "60 s audio: {audio:?} ({:.3}% of a core, {packets} packets); 60 s silence: {silence:?} ({:.3}%)",
            audio.as_secs_f64() / 60.0 * 100.0,
            silence.as_secs_f64() / 60.0 * 100.0
        );
    }

    /// Measures the encoder delay: encodes impulses at known sample positions,
    /// decodes the ADTS stream with ffmpeg (which outputs every decoded sample,
    /// as ADTS carries no priming info) and finds the peaks.
    ///
    /// `cargo test -p rec-audio --lib -- --ignored --nocapture encoder_delay`
    #[test]
    #[ignore = "needs ffmpeg on PATH"]
    fn encoder_delay() {
        let Some((_com, mut enc)) = encoder(160) else {
            return;
        };
        // One impulse inside the very first packet checks the stream start too.
        let positions = [600_usize, 30_000, 70_001, 110_517, 150_999];
        let mut pcm = vec![0i16; 192_000 * 2];
        for &p in &positions {
            pcm[2 * p] = 20_000;
            pcm[2 * p + 1] = 20_000;
        }
        let packets = encode(&mut enc, &pcm);
        let mut adts = Vec::new();
        for p in &packets {
            adts.extend_from_slice(&aac::adts_header(48_000, 2, p.len()).unwrap());
            adts.extend_from_slice(p);
        }
        let path = std::env::temp_dir().join("rec-audio-encoder-delay.aac");
        std::fs::write(&path, &adts).unwrap();
        let out = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&path)
            .args(["-f", "s16le", "-acodec", "pcm_s16le", "-"])
            .output()
            .expect("run ffmpeg");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let left: Vec<i32> = out
            .stdout
            .chunks_exact(4)
            .map(|c| i32::from(i16::from_le_bytes([c[0], c[1]])))
            .collect();
        println!("{} packets, {} decoded samples", packets.len(), left.len());
        let mut delays = Vec::new();
        for &p in &positions {
            // Search up to 4096 samples either side, so a negative delay shows
            // too.
            let start = p.saturating_sub(4096);
            let window = &left[start..p + 4096];
            let (offset, peak) = window
                .iter()
                .enumerate()
                .max_by_key(|(_, v)| v.abs())
                .unwrap();
            let delay = (start + offset) as i64 - p as i64;
            println!("impulse at {p}: decoded peak {peak} at {delay:+}");
            delays.push(delay);
        }
        assert!(delays.iter().all(|&d| d == delays[0]), "{delays:?}");
        assert_eq!(
            delays[0],
            crate::ENCODER_DELAY_SAMPLES,
            "update ENCODER_DELAY_SAMPLES"
        );
    }
}
