//! Which OS capture API records a source (pure, unit-tested).
//!
//! Windows.Graphics.Capture (WGC) is the default: it captures one window
//! even when other windows cover it. But before Windows 11 (build 20348) it
//! cannot turn off the yellow border it draws around the captured window or
//! display, and a border around the game is not acceptable while playing.
//! There, Desktop Duplication (DDA) captures the display the game is on and
//! the pipeline crops it to the game window, so no border is shown (decision
//! D2).

/// The OS capture API.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CaptureApi {
    /// WGC where it can capture without the yellow border, else DDA.
    #[default]
    Auto,
    /// Windows.Graphics.Capture. Before Windows 11 it draws a yellow border
    /// around the captured window or display.
    Wgc,
    /// Desktop Duplication of the source's display, cropped to the window:
    /// no border, but whatever covers the window (notifications, other
    /// windows) is recorded too, and the mouse cursor is not drawn.
    Dda,
}

/// What the system offers, as far as the choice is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Facts {
    /// `GraphicsCaptureSession::IsSupported` (Win10 1903+).
    pub wgc: bool,
    /// The session has an `IsBorderRequired` property (build 20348+).
    pub border_property: bool,
    /// The source's display can be duplicated: DXGI lists it as an output of
    /// the pipeline's GPU, and it is not rotated.
    pub dda: bool,
}

/// The API to try first for `pref`: never [`CaptureApi::Auto`].
pub(crate) fn first_choice(pref: CaptureApi, facts: Facts) -> CaptureApi {
    match pref {
        CaptureApi::Wgc | CaptureApi::Dda => pref,
        CaptureApi::Auto if !facts.wgc => CaptureApi::Dda,
        // A rotated display or one on another GPU: a border beats nothing.
        CaptureApi::Auto if !facts.dda => CaptureApi::Wgc,
        CaptureApi::Auto if !facts.border_property => CaptureApi::Dda,
        CaptureApi::Auto => CaptureApi::Wgc,
    }
}

/// Whether `Auto` should give up the WGC session it just created, which
/// reported `border_required` after rec tried to turn the border off (a
/// packaged build without the capability, or access denied), for DDA.
pub(crate) fn dda_instead(pref: CaptureApi, border_required: Option<bool>, dda: bool) -> bool {
    pref == CaptureApi::Auto && dda && border_required != Some(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIN11: Facts = Facts {
        wgc: true,
        border_property: true,
        dda: true,
    };
    const WIN10: Facts = Facts {
        border_property: false,
        ..WIN11
    };

    #[test]
    fn auto_uses_wgc_where_the_border_can_go() {
        assert_eq!(first_choice(CaptureApi::Auto, WIN11), CaptureApi::Wgc);
        assert!(!dda_instead(CaptureApi::Auto, Some(false), true));
    }

    #[test]
    fn auto_uses_dda_on_windows_10() {
        assert_eq!(first_choice(CaptureApi::Auto, WIN10), CaptureApi::Dda);
        // Windows 10 before 1903 has no WGC at all.
        let old = Facts {
            wgc: false,
            ..WIN10
        };
        assert_eq!(first_choice(CaptureApi::Auto, old), CaptureApi::Dda);
    }

    #[test]
    fn auto_switches_to_dda_when_the_border_stays() {
        assert!(dda_instead(CaptureApi::Auto, Some(true), true));
        // A rotated display (or one on another GPU) keeps WGC, border and all.
        assert!(!dda_instead(CaptureApi::Auto, Some(true), false));
        let rotated = Facts {
            dda: false,
            ..WIN10
        };
        assert_eq!(first_choice(CaptureApi::Auto, rotated), CaptureApi::Wgc);
    }

    #[test]
    fn a_forced_api_is_used_as_is() {
        for facts in [
            WIN11,
            WIN10,
            Facts {
                dda: false,
                ..WIN10
            },
        ] {
            assert_eq!(first_choice(CaptureApi::Wgc, facts), CaptureApi::Wgc);
            assert_eq!(first_choice(CaptureApi::Dda, facts), CaptureApi::Dda);
        }
        assert!(!dda_instead(CaptureApi::Wgc, Some(true), true));
        assert!(!dda_instead(CaptureApi::Wgc, None, true));
    }
}
