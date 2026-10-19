//! Controller combos (`hotkeys.controller_*`): hold buttons such as `LS+RS`
//! on a gamepad to save a clip, add a marker or star the last clip.
//!
//! - **XInput, polled.** XInput has no events, so a thread reads the
//!   connected controllers 30 times a second, and only while a game is running
//!   (decision D7): `app` starts it with the game and stops it after, and
//!   never when no combo is set. Reading is read-only: the game gets every
//!   press as usual (nothing is consumed), so a combo should be one the game
//!   ignores or doesn't mind.
//! - **Held, once.** A combo acts after it has been held for
//!   `hotkeys.controller_hold_ms` (500 ms by default), so pressing the
//!   buttons together in play doesn't trigger it, and only once per hold.
//! - **Cheap.** `XInputGetState` on an empty slot is slow (~16 µs: it looks
//!   for a device each time), so empty slots are checked once a second, and
//!   with no controller connected the thread only wakes for that (too
//!   little to measure: under 0.03% of a core over 60 s). The 30 Hz reads
//!   run only while one is connected; the same loop waking 30 times a
//!   second cost ~0.08% of a core (measured without a controller).
//!   `xinput1_4.dll` (Windows 8 and later; `xinput9_1_0.dll` as a fallback)
//!   is loaded the first time a combo is needed, never at startup.
//! - **Not covered:** the Xbox (Guide) button (public XInput doesn't report
//!   it), and pads that aren't XInput devices: a DualSense, DualShock 4 or
//!   Switch Pro controller works only through Steam Input or DS4Windows.
//!   Windows.Gaming.Input would read those, but a desktop app gets its
//!   input only while it has focus, and the game has it; GameInput needs
//!   a redistributable installed.

use std::sync::OnceLock;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rec_core::config::HotkeyConfig;
use rec_core::controller::{Button, Combo};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
};
use windows::Win32::UI::Input::XboxController::{
    XINPUT_GAMEPAD, XINPUT_GAMEPAD_TRIGGER_THRESHOLD, XINPUT_STATE,
};
use windows::core::{s, w};

use crate::hotkeys::Action;
use crate::log::{info, warning};

/// How often connected controllers are read.
const POLL: Duration = Duration::from_millis(33);
/// How often empty slots are checked for a newly connected controller.
const RESCAN: Duration = Duration::from_secs(1);
/// XInput's controller slots.
const SLOTS: usize = 4;

/// The configured combos and what each does, in config order.
pub fn bindings(config: &HotkeyConfig) -> Vec<(Combo, Action)> {
    let mut bindings = Vec::new();
    bindings.extend(config.controller_save.map(|c| (c, Action::SaveClip)));
    bindings.extend(config.controller_add_marker.map(|c| (c, Action::AddMarker)));
    bindings.extend(config.controller_star_last.map(|c| (c, Action::StarLast)));
    bindings.extend(
        (config.extra_clips.iter())
            .filter_map(|c| Some((c.controller?, Action::SaveSeconds(c.seconds)))),
    );
    // The same combo twice: the first one wins (see `Combos::update`).
    for (i, (combo, action)) in bindings.iter().enumerate() {
        if bindings[..i].iter().any(|(c, _)| c == combo) {
            warning!("controller combo {combo} for {action:?} is already used; ignored");
        }
    }
    bindings
}

/// The buttons down in an XInput reading, as a [`Combo`] mask.
fn buttons(pad: &XINPUT_GAMEPAD) -> u32 {
    let threshold = XINPUT_GAMEPAD_TRIGGER_THRESHOLD.0 as u8;
    let mut held = u32::from(pad.wButtons.0) & Combo::ALL;
    if pad.bLeftTrigger > threshold {
        held |= Button::LeftTrigger.bit();
    }
    if pad.bRightTrigger > threshold {
        held |= Button::RightTrigger.bit();
    }
    held
}

/// What one controller slot is doing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Slot {
    /// The binding being held, and since when (ms).
    held: Option<(usize, u64)>,
    /// A combo acted and hasn't been let go since.
    fired: bool,
}

/// Turns button readings into actions: a combo acts once it has been held
/// for the hold time, and once per hold. Pure logic, fed by [`Poller`] and
/// by the tests.
#[derive(Debug)]
pub struct Combos<A> {
    bindings: Vec<(Combo, A)>,
    hold_ms: u64,
    slots: [Slot; SLOTS],
}

impl<A: Copy> Combos<A> {
    pub fn new(bindings: Vec<(Combo, A)>, hold_ms: u32) -> Self {
        Self {
            bindings,
            hold_ms: u64::from(hold_ms),
            slots: [Slot::default(); SLOTS],
        }
    }

    /// Takes a reading of `slot` at `now_ms` (any monotonic clock):
    /// the buttons down, or `None` when no controller is there. Returns the
    /// action to take, if a combo has just been held long enough.
    ///
    /// When several combos are down (`View+DPadDown` is also `View+...`
    /// with one more button), the one with the most buttons counts, so a
    /// longer combo never also triggers a shorter one inside it. After a
    /// combo acts, nothing more happens on that controller until none is
    /// held: holding on, or pressing another button, doesn't act again.
    pub fn update(&mut self, slot: usize, held: Option<u32>, now_ms: u64) -> Option<A> {
        let state = self.slots.get_mut(slot)?;
        let best = held.and_then(|held| {
            (self.bindings.iter().enumerate())
                .filter(|(_, (combo, _))| combo.is_held(held))
                // Most buttons; the first configured among equals.
                .max_by_key(|&(i, (combo, _))| (combo.len(), std::cmp::Reverse(i)))
                .map(|(i, _)| i)
        });
        let Some(best) = best else {
            *state = Slot::default();
            return None;
        };
        if state.fired {
            return None;
        }
        match state.held {
            Some((i, since)) if i == best => {
                if now_ms.saturating_sub(since) >= self.hold_ms {
                    *state = Slot {
                        held: None,
                        fired: true,
                    };
                    return Some(self.bindings[best].1);
                }
            }
            // Just pressed, or it became a longer combo: start timing.
            _ => state.held = Some((best, now_ms)),
        }
        None
    }

    /// Whether any controller is part-way through holding a combo.
    #[cfg(test)]
    fn holding(&self) -> bool {
        self.slots.iter().any(|s| s.held.is_some())
    }
}

type GetState = unsafe extern "system" fn(u32, *mut XINPUT_STATE) -> u32;

/// `XInputGetState`, loaded on first use; `None` if XInput is missing.
fn xinput() -> Option<GetState> {
    static GET_STATE: OnceLock<Option<GetState>> = OnceLock::new();
    *GET_STATE.get_or_init(|| {
        for dll in [w!("xinput1_4.dll"), w!("xinput9_1_0.dll")] {
            // SAFETY: loading a system DLL by name; it stays loaded for
            // the life of the process, so the function pointer stays valid.
            // XInputGetState has this signature in every XInput version.
            unsafe {
                let Ok(module) = LoadLibraryW(dll) else {
                    continue;
                };
                if let Some(f) = GetProcAddress(module, s!("XInputGetState")) {
                    return Some(std::mem::transmute::<
                        unsafe extern "system" fn() -> isize,
                        GetState,
                    >(f));
                }
            }
        }
        None
    })
}

/// The polling thread; dropping it stops the thread.
#[derive(Debug)]
pub struct Poller {
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Poller {
    /// Starts reading controllers, calling `fire` (on the polling thread)
    /// when a combo is held long enough.
    pub fn start(
        bindings: Vec<(Combo, Action)>,
        hold_ms: u32,
        fire: impl Fn(Action) + Send + 'static,
    ) -> Result<Self, String> {
        let get_state = xinput().ok_or("XInput is not available")?;
        let (stop, stopped) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("controller".into())
            .spawn(move || {
                // SAFETY: lowering this thread's own priority.
                let _ =
                    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
                let mut combos = Combos::new(bindings, hold_ms);
                let start = Instant::now();
                let mut connected = [false; SLOTS];
                let mut next_scan = start;
                let mut wait = Duration::ZERO;
                // Ends when the sender is dropped (or sends).
                while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(wait) {
                    let now = Instant::now();
                    let scan = now >= next_scan;
                    if scan {
                        next_scan = now + RESCAN;
                    }
                    let ms = now.duration_since(start).as_millis() as u64;
                    for (slot, connected) in connected.iter_mut().enumerate() {
                        if !*connected && !scan {
                            continue;
                        }
                        let mut state = XINPUT_STATE::default();
                        // SAFETY: valid slot index and out-pointer.
                        let found = unsafe { get_state(slot as u32, &mut state) } == 0;
                        if found != *connected {
                            info!(
                                "controller {} {}",
                                slot + 1,
                                if found { "connected" } else { "gone" }
                            );
                        }
                        *connected = found;
                        let held = found.then(|| buttons(&state.Gamepad));
                        if let Some(action) = combos.update(slot, held, ms) {
                            info!("controller {}: {action:?}", slot + 1);
                            fire(action);
                        }
                    }
                    // With no controller there is nothing to time: sleep
                    // until the next look for one.
                    wait = if connected.contains(&true) {
                        POLL
                    } else {
                        next_scan.saturating_duration_since(Instant::now())
                    };
                }
            })
            .map_err(|e| format!("could not start the controller thread: {e}"))?;
        Ok(Self {
            stop: Some(stop),
            thread: Some(thread),
        })
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        // Wakes the thread at once; it ends within one read.
        self.stop = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Input::XboxController::*;

    fn combo(s: &str) -> Combo {
        s.parse().unwrap()
    }

    fn mask(s: &str) -> Option<u32> {
        Some(combo(s).mask())
    }

    /// Feeds `(ms, buttons)` readings for slot 0 and returns when each
    /// action fired.
    fn run<A: Copy>(combos: &mut Combos<A>, samples: &[(u64, Option<u32>)]) -> Vec<(u64, A)> {
        samples
            .iter()
            .filter_map(|&(t, held)| combos.update(0, held, t).map(|a| (t, a)))
            .collect()
    }

    #[test]
    fn fires_once_after_the_hold_time() {
        let mut c = Combos::new(vec![(combo("LS+RS"), 'S')], 500);
        let ls_rs = mask("LS+RS");
        let samples: Vec<_> = (0..40).map(|i| (i * 33, ls_rs)).collect();
        // First seen at 0 ms; 528 ms is the first reading at least 500 later.
        assert_eq!(run(&mut c, &samples), vec![(528, 'S')]);

        // Let go and hold again: fires again.
        let again = [(2000, None), (2033, ls_rs), (2500, ls_rs), (2533, ls_rs)];
        assert_eq!(run(&mut c, &again), vec![(2533, 'S')]);
    }

    #[test]
    fn short_presses_and_partial_combos_do_nothing() {
        let mut c = Combos::new(vec![(combo("View+Menu"), 'S')], 500);
        let both = mask("View+Menu");
        let view = mask("View+A").map(|m| m & !Button::A.bit());
        let samples = [
            (0, both),
            (400, both),
            // Released just before the hold time.
            (450, Some(0)),
            (500, both),
            (900, both),
            // One button let go for a reading restarts the hold.
            (950, view),
            (1000, both),
            (1450, both),
        ];
        assert_eq!(run(&mut c, &samples), vec![]);
        assert!(c.holding());
        assert_eq!(run(&mut c, &[(1500, both)]), vec![(1500, 'S')]);
    }

    #[test]
    fn other_buttons_and_sticks_may_be_held_too() {
        let mut c = Combos::new(vec![(combo("LS+RS"), 'S')], 500);
        let busy = mask("LS+RS").map(|m| m | Button::A.bit() | Button::RightTrigger.bit());
        assert_eq!(run(&mut c, &[(0, busy), (600, busy)]), vec![(600, 'S')]);
    }

    #[test]
    fn the_longest_held_combo_wins_and_nothing_fires_twice() {
        let mut c = Combos::new(
            vec![
                (combo("View+Menu"), 'S'),
                (combo("View+Menu+DPadDown"), '1'),
            ],
            500,
        );
        let short = mask("View+Menu");
        let long = mask("View+Menu+DPadDown");
        // The longer combo pressed within the hold: only it fires, timed
        // from when it was complete.
        let samples = [(0, short), (300, long), (600, long), (800, long)];
        assert_eq!(run(&mut c, &samples), vec![(800, '1')]);
        // Back to the shorter one without letting go: nothing more.
        assert_eq!(run(&mut c, &[(900, short), (2000, short)]), vec![]);
        // Let go of everything, then the short one alone.
        let samples = [(2100, Some(0)), (2200, short), (2700, short)];
        assert_eq!(run(&mut c, &samples), vec![(2700, 'S')]);
    }

    #[test]
    fn same_combo_twice_first_wins() {
        let mut c = Combos::new(vec![(combo("LB+RB"), 'a'), (combo("LB+RB"), 'b')], 100);
        let lb_rb = mask("LB+RB");
        assert_eq!(run(&mut c, &[(0, lb_rb), (100, lb_rb)]), vec![(100, 'a')]);
    }

    #[test]
    fn controllers_are_independent_and_disconnecting_resets() {
        let mut c = Combos::new(vec![(combo("LS+RS"), 'S')], 500);
        let ls_rs = mask("LS+RS");
        assert_eq!(c.update(0, ls_rs, 0), None);
        assert_eq!(c.update(3, ls_rs, 200), None);
        assert_eq!(c.update(0, ls_rs, 500), Some('S'));
        // Slot 3 started later.
        assert_eq!(c.update(3, ls_rs, 500), None);
        assert_eq!(c.update(3, ls_rs, 700), Some('S'));
        // Unplugged mid-hold: starts over when it comes back.
        assert_eq!(c.update(1, ls_rs, 0), None);
        assert_eq!(c.update(1, None, 300), None);
        assert_eq!(c.update(1, ls_rs, 400), None);
        assert_eq!(c.update(1, ls_rs, 600), None);
        assert_eq!(c.update(1, ls_rs, 900), Some('S'));
        // Slots past XInput's four are ignored.
        assert_eq!(c.update(4, ls_rs, 0), None);
    }

    #[test]
    fn maps_xinput_readings() {
        let pad = |buttons: XINPUT_GAMEPAD_BUTTON_FLAGS, lt: u8, rt: u8| XINPUT_GAMEPAD {
            wButtons: buttons,
            bLeftTrigger: lt,
            bRightTrigger: rt,
            ..Default::default()
        };
        assert_eq!(
            buttons(&pad(
                XINPUT_GAMEPAD_LEFT_THUMB | XINPUT_GAMEPAD_RIGHT_THUMB,
                0,
                0
            )),
            combo("LS+RS").mask()
        );
        assert_eq!(
            buttons(&pad(XINPUT_GAMEPAD_BACK | XINPUT_GAMEPAD_START, 0, 0)),
            combo("View+Menu").mask()
        );
        assert_eq!(
            buttons(&pad(
                XINPUT_GAMEPAD_DPAD_DOWN | XINPUT_GAMEPAD_Y | XINPUT_GAMEPAD_LEFT_SHOULDER,
                0,
                0
            )),
            combo("LB+DPadDown+Y").mask()
        );
        // Triggers count past XInput's threshold.
        assert_eq!(
            buttons(&pad(XINPUT_GAMEPAD_A, 30, 31)),
            combo("RT+A").mask()
        );
        assert_eq!(
            buttons(&pad(XINPUT_GAMEPAD_A, 255, 0)),
            combo("LT+A").mask()
        );
        // The Guide bit (only from undocumented calls) is dropped.
        assert_eq!(buttons(&pad(XINPUT_GAMEPAD_BUTTON_FLAGS(0x0400), 0, 0)), 0);
    }

    #[test]
    fn bindings_follow_the_config() {
        let config = rec_core::config::Config::from_toml(
            r#"[hotkeys]
controller_save = "LS+RS"
controller_add_marker = "View+DPadUp"
[[hotkeys.extra_clips]]
hotkey = "Alt+F6"
seconds = 15
controller = "View+DPadDown"
[[hotkeys.extra_clips]]
hotkey = "Alt+F5"
seconds = 300
"#,
        )
        .unwrap();
        assert_eq!(
            bindings(&config.hotkeys),
            vec![
                (combo("LS+RS"), Action::SaveClip),
                (combo("View+DPadUp"), Action::AddMarker),
                (combo("View+DPadDown"), Action::SaveSeconds(15)),
            ]
        );
        assert!(bindings(&HotkeyConfig::default()).is_empty());
    }

    /// Runs the real poller for `REC_TEST_SECONDS` (default 10), printing
    /// every combo held (LS+RS, View+Menu, View+DPadDown) and the CPU time
    /// it used. With a controller: hold a combo for half a second.
    /// `cargo test --release -p recd poll_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "runs for seconds; reads real controllers"]
    fn poll_cost() {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

        let cpu = || {
            let (mut a, mut b, mut kernel, mut user) = Default::default();
            // SAFETY: valid out-pointers for our own process.
            unsafe { GetProcessTimes(GetCurrentProcess(), &mut a, &mut b, &mut kernel, &mut user) }
                .unwrap();
            let t = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
            Duration::from_nanos((t(kernel) + t(user)) * 100)
        };
        let seconds: u64 = std::env::var("REC_TEST_SECONDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10);
        let bindings = vec![
            (combo("LS+RS"), Action::SaveClip),
            (combo("View+Menu"), Action::StarLast),
            (combo("View+DPadDown"), Action::SaveSeconds(15)),
        ];
        // What one read costs, on whatever is in slot 0.
        let get_state = xinput().unwrap();
        let t = Instant::now();
        for _ in 0..100 {
            let mut state = XINPUT_STATE::default();
            // SAFETY: valid slot index and out-pointer.
            unsafe { get_state(0, &mut state) };
        }
        println!("one read of slot 0: {:?}", t.elapsed() / 100);

        // The test process alone, for comparison.
        let before = cpu();
        std::thread::sleep(Duration::from_secs(seconds));
        println!(
            "idle {seconds} s: {:.2} ms CPU",
            (cpu() - before).as_secs_f64() * 1000.0
        );

        let before = cpu();
        let started = Instant::now();
        let poller = Poller::start(bindings, 500, move |action| {
            println!("{:>6} ms  {action:?}", started.elapsed().as_millis());
        })
        .unwrap();
        std::thread::sleep(Duration::from_secs(seconds));
        drop(poller);
        let used = cpu() - before;
        let wall = started.elapsed();
        println!(
            "polled {:.1} s: {:.2} ms CPU, {:.4}% of one core",
            wall.as_secs_f64(),
            used.as_secs_f64() * 1000.0,
            used.as_secs_f64() / wall.as_secs_f64() * 100.0
        );
    }

    /// Loads XInput and reads every slot once, like the polling thread.
    /// Passes with no controller connected.
    #[test]
    fn xinput_loads_and_reads() {
        let get_state = xinput().expect("XInput");
        for slot in 0..SLOTS as u32 {
            let mut state = XINPUT_STATE::default();
            // SAFETY: valid slot index and out-pointer.
            let result = unsafe { get_state(slot, &mut state) };
            // ERROR_SUCCESS or ERROR_DEVICE_NOT_CONNECTED.
            assert!(result == 0 || result == 1167, "slot {slot}: {result}");
        }
    }
}
