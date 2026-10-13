//! Notices when a game that is being recorded is not really being captured,
//! so the user hears about it now instead of finding black or frozen clips
//! later (instant replay that silently fails is the top complaint about
//! every clipper).
//!
//! The capture APIs give no error in these cases, so the watcher judges from
//! what arrives, but only while the game is in the foreground, not minimized
//! and covering its monitor; that is when a game renders continuously and a
//! picture is expected. Measured on Windows 11 26200, AMD (see the
//! `fake_game` example in rec-video):
//! - **Fullscreen optimizations** turn a DXGI fullscreen game (flip or blt
//!   model) into a composed one: it is captured normally, while the shell
//!   reports "D3D fullscreen" (`QUNS_RUNNING_D3D_FULL_SCREEN`).
//! - **True exclusive fullscreen** (fullscreen optimizations off, here with
//!   `__COMPAT_LAYER=DisableDXMaximizedWindowedMode`): capture keeps
//!   delivering frames, at a reduced rate, but they are black apart from
//!   the cursor. The shell reports the same "D3D fullscreen", so black
//!   pictures while it does are the signal.
//! - **Frames stopping** altogether while the game is in front was seen once
//!   with a GPU in a bad state; it gets the same advice.
//! - **Display affinity** (`SetWindowDisplayAffinity`): the game hides its
//!   window from capture on purpose; nothing can be done about it.
//!
//! The thresholds are long enough for loading screens: a game that stops
//! presenting for a few seconds, or fades to black, is not a problem.

use rec_core::time::{HNS_PER_SEC, Hns};

/// No new frame for this long while the game is in front.
pub const STALL: Hns = 10 * HNS_PER_SEC;
/// A black picture for this long while the shell reports D3D fullscreen:
/// exclusive fullscreen.
pub const BLACK_FULLSCREEN: Hns = 15 * HNS_PER_SEC;
/// A black picture for this long otherwise.
pub const BLACK: Hns = 30 * HNS_PER_SEC;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    /// True exclusive fullscreen: black pictures, or none at all.
    Exclusive,
    /// Only black pictures arrive.
    Black,
    /// The window excludes itself from capture.
    Blocked,
}

impl Problem {
    /// Toast title and advice, in words a user can act on.
    pub fn describe(self, game: &str) -> (String, String) {
        match self {
            Problem::Exclusive => (
                format!("Can't see {game}"),
                "It seems to run in exclusive fullscreen, which Windows doesn't let other \
                 apps capture. Switch it to borderless or windowed fullscreen in its display \
                 settings."
                    .into(),
            ),
            Problem::Black => (
                format!("{game} records as black"),
                "rec only gets a black picture from it. If it runs in exclusive fullscreen, \
                 switch it to borderless or windowed fullscreen."
                    .into(),
            ),
            Problem::Blocked => (
                format!("{game} blocks capture"),
                "The game hides its window from screen capture, so clips will be black. \
                 rec can't change this."
                    .into(),
            ),
        }
    }

    /// One line for the tray tooltip and `rec status`.
    pub fn short(self) -> &'static str {
        match self {
            Problem::Exclusive => "no picture: switch the game to borderless fullscreen",
            Problem::Black => "the picture is black",
            Problem::Blocked => "the game blocks capture",
        }
    }
}

/// What the watcher sees on one check.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Observation {
    pub now: Hns,
    /// QPC time of the newest captured frame.
    pub last_frame: Option<Hns>,
    /// Since when the picture has been black.
    pub black_since: Option<Hns>,
    /// The game window is the foreground window and not minimized.
    pub in_front: bool,
    pub covers_monitor: bool,
    /// The window's display affinity hides it from capture.
    pub blocks_capture: bool,
    /// The shell reports a D3D fullscreen app.
    pub d3d_fullscreen: bool,
}

/// Judges a session's capture over time; one per session.
#[derive(Clone, Debug)]
pub struct Watcher {
    started: Hns,
    /// Since when the game has been in front and fullscreen.
    in_front_since: Option<Hns>,
}

impl Watcher {
    pub fn new(started: Hns) -> Self {
        Self {
            started,
            in_front_since: None,
        }
    }

    pub fn check(&mut self, o: &Observation) -> Option<Problem> {
        if o.blocks_capture {
            return Some(Problem::Blocked);
        }
        if !(o.in_front && o.covers_monitor) {
            self.in_front_since = None;
            return None;
        }
        // Only time in front counts: a minimized game sends no frames either.
        let since = *self.in_front_since.get_or_insert(o.now);
        let from = |t: Option<Hns>| t.unwrap_or(self.started).max(since).max(self.started);
        let black_for = o.black_since.map(|t| o.now - from(Some(t)));
        if o.now - from(o.last_frame) >= STALL
            || (o.d3d_fullscreen && black_for.is_some_and(|d| d >= BLACK_FULLSCREEN))
        {
            Some(Problem::Exclusive)
        } else if black_for.is_some_and(|d| d >= BLACK) {
            Some(Problem::Black)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Hns = HNS_PER_SEC;

    fn front(now: Hns, last_frame: Option<Hns>) -> Observation {
        Observation {
            now,
            last_frame,
            in_front: true,
            covers_monitor: true,
            ..Observation::default()
        }
    }

    #[test]
    fn frames_arriving_are_fine() {
        let mut w = Watcher::new(0);
        for t in (2..60).step_by(2) {
            assert_eq!(w.check(&front(t * S, Some(t * S - S / 60))), None);
        }
    }

    #[test]
    fn no_frames_in_front_is_exclusive_fullscreen() {
        let mut w = Watcher::new(0);
        assert_eq!(w.check(&front(2 * S, None)), None);
        assert_eq!(w.check(&front(8 * S, None)), None);
        // Counted from when the game came to the front (2 s).
        assert_eq!(w.check(&front(12 * S, None)), Some(Problem::Exclusive));
        // A frame arrives: fine again.
        assert_eq!(w.check(&front(14 * S, Some(14 * S))), None);
    }

    #[test]
    fn a_loading_hitch_is_not_a_problem() {
        let mut w = Watcher::new(0);
        assert_eq!(w.check(&front(2 * S, Some(2 * S))), None);
        assert_eq!(w.check(&front(9 * S, Some(2 * S))), None);
        assert_eq!(w.check(&front(11 * S, Some(11 * S))), None);
    }

    #[test]
    fn time_in_the_background_does_not_count() {
        let mut w = Watcher::new(0);
        assert_eq!(w.check(&front(2 * S, Some(2 * S))), None);
        // Alt-tabbed out for a minute: minimized, no frames.
        let away = Observation {
            in_front: false,
            ..front(62 * S, Some(2 * S))
        };
        assert_eq!(w.check(&away), None);
        // Back in front: the stall is counted from now.
        assert_eq!(w.check(&front(64 * S, Some(2 * S))), None);
        assert_eq!(w.check(&front(73 * S, Some(2 * S))), None);
        assert_eq!(
            w.check(&front(74 * S, Some(2 * S))),
            Some(Problem::Exclusive)
        );
    }

    #[test]
    fn windowed_games_are_not_judged() {
        let mut w = Watcher::new(0);
        let windowed = Observation {
            covers_monitor: false,
            ..front(100 * S, None)
        };
        assert_eq!(w.check(&windowed), None);
    }

    #[test]
    fn long_black_is_reported_and_fades_are_not() {
        let mut w = Watcher::new(0);
        let black = |now, since| Observation {
            black_since: Some(since),
            ..front(now, Some(now))
        };
        assert_eq!(w.check(&black(2 * S, 2 * S)), None);
        assert_eq!(w.check(&black(20 * S, 2 * S)), None);
        assert_eq!(w.check(&black(32 * S, 2 * S)), Some(Problem::Black));
        assert_eq!(w.check(&front(34 * S, Some(34 * S))), None);
    }

    #[test]
    fn black_in_d3d_fullscreen_is_exclusive_sooner() {
        let mut w = Watcher::new(0);
        let black = |now| Observation {
            black_since: Some(2 * S),
            d3d_fullscreen: true,
            ..front(now, Some(now))
        };
        assert_eq!(w.check(&black(2 * S)), None);
        assert_eq!(w.check(&black(16 * S)), None);
        assert_eq!(w.check(&black(17 * S)), Some(Problem::Exclusive));
    }

    #[test]
    fn display_affinity_is_reported_even_in_the_background() {
        let mut w = Watcher::new(0);
        let blocked = Observation {
            blocks_capture: true,
            in_front: false,
            ..front(2 * S, Some(2 * S))
        };
        assert_eq!(w.check(&blocked), Some(Problem::Blocked));
    }
}
