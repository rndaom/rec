//! What the display showing a capture source does with colour: whether
//! Windows HDR is on for it, its luminance and primaries (the HDR10
//! "mastering display" of a clip recorded from it), and how bright Windows
//! draws SDR content on it (the reference white when tone-mapping to SDR).
//!
//! HDR state comes from DXGI (`IDXGIOutput6::GetDesc1` reports the PQ /
//! BT.2020 colour space while HDR is on), the SDR white level from the
//! display configuration API. Both are cheap enough to ask once a second from
//! recd's own thread, so the encode thread never does it.

use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SDR_WHITE_LEVEL, DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo,
    GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
use windows::Win32::Graphics::Dxgi::{IDXGIOutput, IDXGIOutput6};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFOEXW};
use windows::core::Interface;

use rec_core::media::HdrMetadata;

use crate::d3d::{find_adapter, luid_key, target_monitor};
use crate::util::{Context, from_wide};
use crate::{CaptureSource, VideoError};

/// Reference white Windows uses for SDR content when the display reports
/// nothing (the "SDR content brightness" slider's middle is about this).
const DEFAULT_SDR_WHITE_NITS: f32 = 200.0;

/// Colour facts about one display.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DisplayColor {
    /// Windows HDR is on for this display.
    pub hdr: bool,
    /// Peak, minimum and full-frame luminance in cd/m² (0 = not reported).
    pub max_nits: f32,
    pub min_nits: f32,
    pub max_full_frame_nits: f32,
    /// CIE 1931 xy chromaticities of the panel.
    pub red: [f32; 2],
    pub green: [f32; 2],
    pub blue: [f32; 2],
    pub white: [f32; 2],
    /// How bright Windows draws SDR white on this display in HDR mode, in
    /// cd/m² (80 when HDR is off).
    pub sdr_white_nits: f32,
}

impl DisplayColor {
    /// Peak luminance to clip to, cd/m²: the panel's, or 1000 when it
    /// reports none.
    pub fn peak_nits(&self) -> f32 {
        if self.max_nits >= 80.0 {
            self.max_nits
        } else {
            1000.0
        }
    }

    /// HDR10 static metadata for a clip recorded from this display.
    pub fn metadata(&self) -> HdrMetadata {
        let reported = self.max_nits >= 80.0 && self.red[0] > 0.0 && self.white[0] > 0.0;
        if !reported {
            return HdrMetadata::bt2020_1000();
        }
        HdrMetadata::from_display(
            self.red,
            self.green,
            self.blue,
            self.white,
            self.max_nits,
            self.min_nits,
            self.max_full_frame_nits,
        )
    }

    /// `display` pretended to be in HDR mode, to test the HDR path on an SDR
    /// display: its own luminance and primaries where known. SDR content on
    /// it is 80 cd/m² white, as scRGB captures of SDR displays are.
    pub(crate) fn assume_hdr(display: Option<Self>) -> Self {
        let mut d = display.unwrap_or(Self {
            hdr: true,
            max_nits: 0.0,
            min_nits: 0.0,
            max_full_frame_nits: 0.0,
            red: [0.0; 2],
            green: [0.0; 2],
            blue: [0.0; 2],
            white: [0.0; 2],
            sdr_white_nits: 80.0,
        });
        d.hdr = true;
        d.sdr_white_nits = d.sdr_white_nits.max(80.0);
        d
    }
}

/// The display a capture source is on, as seen right now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DisplayState {
    /// LUID of the adapter driving it; see [`crate::adapter_for`].
    pub adapter_luid: i64,
    /// `None` for displays DXGI does not list (indirect or virtual ones).
    pub color: Option<DisplayColor>,
}

/// Looks up the display `source` is on: its adapter and colour state.
pub fn display_for(source: CaptureSource) -> Result<DisplayState, VideoError> {
    let monitor = target_monitor(source);
    let (adapter, output) = find_adapter(monitor)?;
    // SAFETY: GetDesc1 fills a plain struct.
    let desc = unsafe { adapter.GetDesc1() }.ctx("IDXGIAdapter1::GetDesc1")?;
    Ok(DisplayState {
        adapter_luid: luid_key(desc.AdapterLuid),
        color: output.and_then(|o| color_of(&o, monitor)),
    })
}

fn color_of(output: &IDXGIOutput, monitor: HMONITOR) -> Option<DisplayColor> {
    let output6: IDXGIOutput6 = output.cast().ok()?;
    // SAFETY: GetDesc1 fills a plain struct.
    let d = unsafe { output6.GetDesc1() }.ok()?;
    let hdr = d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
    let sdr_white_nits = if hdr {
        sdr_white_nits(monitor).unwrap_or(DEFAULT_SDR_WHITE_NITS)
    } else {
        80.0
    };
    Some(DisplayColor {
        hdr,
        max_nits: d.MaxLuminance,
        min_nits: d.MinLuminance,
        max_full_frame_nits: d.MaxFullFrameLuminance,
        red: d.RedPrimary,
        green: d.GreenPrimary,
        blue: d.BluePrimary,
        white: d.WhitePoint,
        sdr_white_nits,
    })
}

/// The SDR white level of `monitor`'s display path, in cd/m².
fn sdr_white_nits(monitor: HMONITOR) -> Option<f32> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: `info` is a correctly sized MONITORINFOEXW; an invalid
    // HMONITOR just makes the call fail.
    if !unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) }.as_bool() {
        return None;
    }
    let gdi_name = from_wide(&info.szDevice);

    let (mut path_count, mut mode_count) = (0u32, 0u32);
    // SAFETY: out-pointers valid for writes.
    if unsafe {
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
    } != ERROR_SUCCESS
    {
        return None;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
    // SAFETY: the buffers hold the counts passed in; the call updates the
    // counts to what it wrote.
    if unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        )
    } != ERROR_SUCCESS
    {
        return None;
    }
    paths.truncate(path_count as usize);

    for path in &paths {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: the header gives the type and size of the struct it heads.
        if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != 0
            || from_wide(&source.viewGdiDeviceName) != gdi_name
        {
            continue;
        }
        let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                size: size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            SDRWhiteLevel: 0,
        };
        // SAFETY: as above.
        if unsafe { DisplayConfigGetDeviceInfo(&mut white.header) } != 0 {
            return None;
        }
        // 1000 means 80 cd/m².
        let nits = white.SDRWhiteLevel as f32 * 80.0 / 1000.0;
        return (nits >= 80.0).then_some(nits);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel() -> DisplayColor {
        DisplayColor {
            hdr: true,
            max_nits: 603.7,
            min_nits: 0.0,
            max_full_frame_nits: 276.8,
            red: [0.6768, 0.3213],
            green: [0.2656, 0.6611],
            blue: [0.1504, 0.0596],
            white: [0.3135, 0.3291],
            sdr_white_nits: 240.0,
        }
    }

    #[test]
    fn metadata_comes_from_the_panel() {
        let m = panel().metadata();
        assert_eq!(m.max_cll, 604);
        assert_eq!(m.max_fall, 277);
        assert_eq!(m.red, [33_840, 16_065]);
    }

    #[test]
    fn unreported_values_fall_back_to_conventional_ones() {
        let blank = DisplayColor {
            max_nits: 0.0,
            red: [0.0; 2],
            ..panel()
        };
        assert_eq!(blank.metadata(), HdrMetadata::bt2020_1000());
        assert_eq!(blank.peak_nits(), 1000.0);
        assert_eq!(panel().peak_nits(), 603.7);
    }

    #[test]
    #[ignore = "needs a display"]
    fn primary_display_reports_its_colour() {
        let state = display_for(CaptureSource::PrimaryMonitor).unwrap();
        let color = state.color.expect("primary display listed by DXGI");
        println!("{color:?}");
        assert!(color.sdr_white_nits >= 80.0);
    }
}
