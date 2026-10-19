//! Output-size and letterbox arithmetic (pure, unit-tested).

/// An integer rectangle, `right`/`bottom` exclusive (same as Win32 `RECT`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

impl Rect {
    pub fn width(&self) -> u32 {
        self.right - self.left
    }

    pub fn height(&self) -> u32 {
        self.bottom - self.top
    }
}

/// A rectangle in virtual-screen coordinates, which can be negative (a
/// monitor left of the primary one), `right`/`bottom` exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// The part of `window` that `output` shows, relative to the output's
/// top-left corner: what Desktop Duplication crops from the output's desktop
/// image. `None` when less than 2 x 2 pixels of the window are on the output
/// (minimised windows sit at -32000, or the window moved to another display).
pub fn crop(window: ScreenRect, output: ScreenRect) -> Option<Rect> {
    let left = i64::from(window.left.max(output.left));
    let top = i64::from(window.top.max(output.top));
    let right = i64::from(window.right.min(output.right));
    let bottom = i64::from(window.bottom.min(output.bottom));
    if right - left < 2 || bottom - top < 2 {
        return None;
    }
    let (ox, oy) = (i64::from(output.left), i64::from(output.top));
    Some(Rect {
        left: (left - ox) as u32,
        top: (top - oy) as u32,
        right: (right - ox) as u32,
        bottom: (bottom - oy) as u32,
    })
}

/// Rounds down to an even number, never below 2. NV12 and the encoders need
/// even dimensions.
pub fn even(v: u32) -> u32 {
    (v & !1).max(2)
}

/// Encoded size for a session: the source size, scaled down (aspect ratio
/// kept) so the height is at most `max_height`, rounded to even numbers.
pub fn output_size(src_width: u32, src_height: u32, max_height: Option<u32>) -> (u32, u32) {
    let (w, h) = (src_width.max(2), src_height.max(2));
    match max_height {
        Some(max_h) if h > max_h => {
            let max_h = even(max_h);
            let scaled_w = (u64::from(w) * u64::from(max_h) + u64::from(h) / 2) / u64::from(h);
            (even(scaled_w as u32), max_h)
        }
        _ => (even(w), even(h)),
    }
}

/// `v` rounded up to a multiple of `align` (a power of two).
pub fn align_up(v: u32, align: u32) -> u32 {
    debug_assert!(align.is_power_of_two());
    v.saturating_add(align - 1) & !(align - 1)
}

/// Where a `src_width` x `src_height` image lands in a coded frame of
/// `coded` size whose visible picture is `picture` (centred, the rest padding
/// an encoder needs for alignment): letterboxed into the picture, then
/// offset into the frame. Without padding this is [`letterbox`].
pub fn place(src_width: u32, src_height: u32, picture: (u32, u32), coded: (u32, u32)) -> Rect {
    let (pw, ph) = (picture.0.min(coded.0), picture.1.min(coded.1));
    let r = letterbox(src_width, src_height, pw, ph);
    let dx = ((coded.0 - pw) / 2) & !1;
    let dy = ((coded.1 - ph) / 2) & !1;
    Rect {
        left: r.left + dx,
        top: r.top + dy,
        right: r.right + dx,
        bottom: r.bottom + dy,
    }
}

/// Where a `src_width` x `src_height` image lands inside a `dst_width` x
/// `dst_height` frame when scaled to fit with its aspect ratio kept. The rest
/// of the frame is black bars. Offsets and sizes are even so NV12 chroma
/// samples never straddle the picture edge.
pub fn letterbox(src_width: u32, src_height: u32, dst_width: u32, dst_height: u32) -> Rect {
    let (sw, sh) = (u64::from(src_width.max(1)), u64::from(src_height.max(1)));
    let (dw, dh) = (u64::from(dst_width), u64::from(dst_height));
    let (w, h) = if sw * dh >= sh * dw {
        // Source is relatively wider: fill the width, bars top and bottom.
        (dw, (sh * dw + sw / 2) / sw)
    } else {
        // Source is relatively taller: fill the height, bars left and right.
        ((sw * dh + sh / 2) / sh, dh)
    };
    let w = even(w.min(dw) as u32).min(dst_width);
    let h = even(h.min(dh) as u32).min(dst_height);
    let left = ((dst_width - w) / 2) & !1;
    let top = ((dst_height - h) / 2) & !1;
    Rect {
        left,
        top,
        right: left + w,
        bottom: top + h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn screen(left: i32, top: i32, right: i32, bottom: i32) -> ScreenRect {
        ScreenRect {
            left,
            top,
            right,
            bottom,
        }
    }

    const PRIMARY: ScreenRect = screen(0, 0, 2560, 1440);
    /// A 1080p monitor left of the primary one.
    const LEFT: ScreenRect = screen(-1920, 0, 0, 1080);

    #[test]
    fn crop_is_relative_to_the_output() {
        // Borderless fullscreen: the whole output.
        assert_eq!(
            crop(PRIMARY, PRIMARY),
            Some(Rect {
                left: 0,
                top: 0,
                right: 2560,
                bottom: 1440
            })
        );
        // A window on the left monitor.
        let r = crop(screen(-1500, 100, -220, 820), LEFT).unwrap();
        assert_eq!(
            (r.left, r.top, r.width(), r.height()),
            (420, 100, 1280, 720)
        );
    }

    #[test]
    fn crop_clips_to_the_output() {
        // Hanging off the bottom-right corner.
        let r = crop(screen(2000, 1000, 2900, 1700), PRIMARY).unwrap();
        assert_eq!((r.left, r.top, r.right, r.bottom), (2000, 1000, 2560, 1440));
        // Straddling two monitors: only this output's part.
        let r = crop(screen(-400, 0, 400, 600), PRIMARY).unwrap();
        assert_eq!((r.left, r.width()), (0, 400));
        let r = crop(screen(-400, 0, 400, 600), LEFT).unwrap();
        assert_eq!((r.left, r.right), (1520, 1920));
    }

    #[test]
    fn crop_of_a_window_not_on_the_output_is_none() {
        // Minimised windows are parked at -32000.
        assert_eq!(crop(screen(-32000, -32000, -31840, -31972), PRIMARY), None);
        assert_eq!(crop(screen(100, 100, 900, 700), LEFT), None);
        // Touching edges and slivers.
        assert_eq!(crop(screen(2560, 0, 3000, 600), PRIMARY), None);
        assert_eq!(crop(screen(2559, 0, 3000, 600), PRIMARY), None);
        assert_eq!(crop(screen(10, 10, 10, 600), PRIMARY), None);
        assert!(crop(screen(2558, 0, 3000, 600), PRIMARY).is_some());
        // Extreme coordinates do not overflow.
        assert_eq!(
            crop(screen(i32::MIN, i32::MIN, i32::MAX, i32::MAX), PRIMARY),
            crop(PRIMARY, PRIMARY)
        );
    }

    #[test]
    fn align_up_rounds_to_the_next_multiple() {
        assert_eq!(align_up(1080, 16), 1088);
        assert_eq!(align_up(1088, 16), 1088);
        assert_eq!(align_up(946, 64), 960);
        assert_eq!(align_up(0, 64), 0);
        assert_eq!(align_up(u32::MAX, 64), !63);
    }

    #[test]
    fn place_centres_the_picture_in_the_padded_frame() {
        // 1080p content in a 1088-high AV1 frame: 1:1, 4 rows each side.
        assert_eq!(
            place(1920, 1080, (1920, 1080), (1920, 1088)),
            Rect {
                left: 0,
                top: 4,
                right: 1920,
                bottom: 1084
            }
        );
        // An odd window in a 64x16-aligned frame: not stretched.
        let r = place(946, 532, (946, 532), (960, 544));
        assert_eq!((r.width(), r.height()), (946, 532));
        assert_eq!((r.left, r.top), (6, 6));
        // No padding: plain letterbox.
        assert_eq!(
            place(1280, 720, (1920, 1080), (1920, 1080)),
            letterbox(1280, 720, 1920, 1080)
        );
    }

    #[test]
    fn output_size_keeps_source_when_uncapped() {
        assert_eq!(output_size(2560, 1440, None), (2560, 1440));
        assert_eq!(output_size(2560, 1440, Some(1440)), (2560, 1440));
        assert_eq!(output_size(1921, 1081, None), (1920, 1080));
        assert_eq!(output_size(0, 0, None), (2, 2));
    }

    #[test]
    fn output_size_caps_height_and_keeps_aspect() {
        assert_eq!(output_size(3840, 2160, Some(1080)), (1920, 1080));
        assert_eq!(output_size(2560, 1440, Some(720)), (1280, 720));
        // Portrait monitor.
        assert_eq!(output_size(1080, 1920, Some(1080)), (608, 1080));
        // Odd cap is rounded down to even.
        assert_eq!(output_size(1920, 1080, Some(721)), (1280, 720));
        // Ultrawide 21:9.
        assert_eq!(output_size(3440, 1440, Some(1080)), (2580, 1080));
    }

    #[test]
    fn letterbox_same_aspect_fills_frame() {
        let r = letterbox(3840, 2160, 1920, 1080);
        assert_eq!(
            r,
            Rect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080
            }
        );
    }

    #[test]
    fn letterbox_wider_source_gets_bars_top_and_bottom() {
        // 21:9 content in a 16:9 frame.
        let r = letterbox(3440, 1440, 1920, 1080);
        assert_eq!(r.width(), 1920);
        assert_eq!(r.height(), 804);
        assert_eq!(r.top, 138);
        assert_eq!(r.top % 2, 0);
        assert_eq!(r.left, 0);
    }

    #[test]
    fn letterbox_taller_source_gets_bars_left_and_right() {
        // A 4:3 window in a 16:9 frame.
        let r = letterbox(1024, 768, 1920, 1080);
        assert_eq!(
            r,
            Rect {
                left: 240,
                top: 0,
                right: 1680,
                bottom: 1080
            }
        );
        // A window resized to a sliver.
        let r = letterbox(10, 1000, 1920, 1080);
        assert_eq!(r.height(), 1080);
        assert!(r.width() >= 2 && r.width() % 2 == 0);
        assert!(r.right <= 1920);
    }

    #[test]
    fn letterbox_never_exceeds_frame() {
        for (sw, sh) in [
            (1, 1),
            (7, 3),
            (4097, 2),
            (2, 4097),
            (1919, 1081),
            (640, 480),
        ] {
            let r = letterbox(sw, sh, 1280, 720);
            assert!(r.right <= 1280 && r.bottom <= 720, "{sw}x{sh} -> {r:?}");
            assert!(r.left % 2 == 0 && r.top % 2 == 0 && r.width() % 2 == 0 && r.height() % 2 == 0);
        }
    }
}
