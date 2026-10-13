//! Notices when the captured picture is black, so recd can warn that a game
//! is not really being captured (instead of saving black clips silently).
//!
//! Every [`PROBE_EVERY`] the NV12 picture just converted is copied into a
//! staging texture, and on a later tick, once the GPU has finished the copy
//! (`D3D11_MAP_FLAG_DO_NOT_WAIT`, so the encode thread never waits), a
//! sparse grid of luma samples is read. Cost: one GPU copy and ~32k byte
//! reads every two seconds at 1080p.
//!
//! P010 (HDR10) pictures are read through the high byte of each 16-bit
//! sample: the 10-bit code sits in the top bits, so the high byte is the
//! code at 8-bit scale, and the limited-range black level lines up.
//!
//! True exclusive fullscreen (Windows 11 26200, AMD) shows up exactly like
//! this: capture keeps delivering frames, fewer of them, all black apart
//! from the cursor. recd's `watch` module turns that into advice.

use rec_core::time::{HNS_PER_SEC, Hns};

/// How often a picture is checked.
pub(crate) const PROBE_EVERY: Hns = 2 * HNS_PER_SEC;
/// Sample every this many pixels and rows.
const STEP: usize = 8;
/// Studio-range black is 16; allow for dithering and scaler ringing.
const BLACK_MAX_LUMA: u8 = 24;
/// Samples per this many that may be brighter: the mouse cursor (drawn into
/// the capture) or a tiny overlay on a black picture still counts as black.
const BRIGHT_ALLOWED_PER: usize = 1000;

/// Whether a luma plane (`pitch` bytes per row, `bytes` per little-endian
/// sample) is black, apart from a few samples.
pub(crate) fn is_black(
    plane: &[u8],
    pitch: usize,
    width: usize,
    height: usize,
    bytes: usize,
) -> bool {
    let (mut samples, mut bright) = (0, 0);
    for y in (0..height).step_by(STEP) {
        let Some(row) = plane.get(y * pitch..y * pitch + width * bytes) else {
            break;
        };
        // The most significant byte of each sample.
        for &v in row.iter().skip(bytes - 1).step_by(STEP * bytes) {
            samples += 1;
            bright += usize::from(v > BLACK_MAX_LUMA);
        }
    }
    samples > 0 && bright * BRIGHT_ALLOWED_PER <= samples
}

#[cfg(windows)]
pub(crate) use imp::BlackProbe;

#[cfg(windows)]
mod imp {
    use std::sync::atomic::Ordering;

    use rec_core::time::Hns;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ,
        D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, ID3D11Device,
        ID3D11DeviceContext, ID3D11Texture2D,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_P010, DXGI_SAMPLE_DESC};

    use super::{PROBE_EVERY, is_black};
    use crate::SharedStats;
    use crate::VideoError;
    use crate::util::Context;

    pub(crate) struct BlackProbe {
        staging: ID3D11Texture2D,
        width: u32,
        height: u32,
        /// Bytes per luma sample: 1 (NV12) or 2 (P010).
        bytes: usize,
        /// A copy is in flight.
        copied: bool,
        next: Hns,
        /// Time of the first black picture in the current run of them.
        black_since: Option<Hns>,
    }

    impl BlackProbe {
        pub(crate) fn new(
            device: &ID3D11Device,
            width: u32,
            height: u32,
            format: DXGI_FORMAT,
        ) -> Result<Self, VideoError> {
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
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging = None;
            // SAFETY: valid descriptor; out-pointer valid for writes.
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }
                .ctx("CreateTexture2D(probe)")?;
            Ok(Self {
                staging: staging
                    .ok_or_else(|| VideoError::Other("CreateTexture2D returned null".into()))?,
                width,
                height,
                bytes: if format == DXGI_FORMAT_P010 { 2 } else { 1 },
                copied: false,
                next: 0,
                black_since: None,
            })
        }

        /// Called after `picture` (a texture of the probe's size and format) was
        /// converted for the tick at `time`: copies it if a check is due.
        pub(crate) fn offer(
            &mut self,
            context: &ID3D11DeviceContext,
            picture: &ID3D11Texture2D,
            time: Hns,
        ) {
            if self.copied || time < self.next {
                return;
            }
            // SAFETY: same size and format, both on this device; the
            // immediate context is multithread-protected.
            unsafe { context.CopyResource(&self.staging, picture) };
            self.copied = true;
            self.next = time + PROBE_EVERY;
        }

        /// Reads a finished copy, if any, and publishes the result.
        pub(crate) fn poll(
            &mut self,
            context: &ID3D11DeviceContext,
            time: Hns,
            stats: &SharedStats,
        ) {
            if !self.copied {
                return;
            }
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            // SAFETY: our staging texture; DO_NOT_WAIT fails instead of
            // blocking while the copy is still running.
            let map = unsafe {
                context.Map(
                    &self.staging,
                    0,
                    D3D11_MAP_READ,
                    D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                    Some(&mut mapped),
                )
            };
            if map.is_err() {
                // Still drawing (or a transient failure): try next tick.
                return;
            }
            let pitch = mapped.RowPitch as usize;
            let (width, height) = (self.width as usize, self.height as usize);
            // SAFETY: while mapped, the Y plane is `height` rows of `pitch`
            // bytes at pData (the UV plane follows it).
            let plane =
                unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, pitch * height) };
            let black = is_black(plane, pitch, width, height, self.bytes);
            // SAFETY: mapped above.
            unsafe { context.Unmap(&self.staging, 0) };
            self.copied = false;
            self.black_since = if black {
                Some(self.black_since.unwrap_or(time))
            } else {
                None
            };
            stats
                .black_since
                .store(self.black_since.unwrap_or(0), Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn studio_black_with_noise_is_black() {
        let (w, h, pitch) = (64, 32, 80);
        let mut plane = vec![16u8; pitch * h];
        plane[3] = 20;
        plane[pitch * 8 + 16] = 24;
        // Padding past the width is ignored.
        plane[pitch - 1] = 255;
        assert!(is_black(&plane, pitch, w, h, 1));
    }

    #[test]
    fn a_cursor_on_black_is_black_but_content_is_not() {
        let (w, h) = (1920, 1080);
        let mut plane = vec![16u8; w * h];
        // A 32x32 cursor.
        for y in 500..532 {
            plane[y * w + 900..y * w + 932].fill(235);
        }
        assert!(is_black(&plane, w, w, h, 1));
        // A 200x100 lit HUD panel.
        for y in 100..200 {
            plane[y * w + 100..y * w + 300].fill(120);
        }
        assert!(!is_black(&plane, w, w, h, 1));
    }

    #[test]
    fn a_dark_scene_is_not_black() {
        let (w, h) = (64, 64);
        let plane: Vec<u8> = (0..w * h).map(|i| 18 + (i % 13) as u8).collect();
        assert!(!is_black(&plane, w, w, h, 1));
    }

    #[test]
    fn p010_black_reads_the_high_byte() {
        let (w, h) = (64, 32);
        // 10-bit limited-range black (64) in the top bits: 64 << 6.
        let black = (64u16 << 6).to_le_bytes();
        let mut plane: Vec<u8> = black.repeat(w * h);
        assert!(is_black(&plane, w * 2, w, h, 2));
        // Mid grey (512 << 6) everywhere in the left half.
        let grey = (512u16 << 6).to_le_bytes();
        for y in 0..h {
            for x in 0..w / 2 {
                plane[(y * w + x) * 2..][..2].copy_from_slice(&grey);
            }
        }
        assert!(!is_black(&plane, w * 2, w, h, 2));
    }
}
