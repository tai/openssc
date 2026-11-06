//! Portable release-chord state machine for the Linux evdev keyboard grab.
//!
//! The chord is LeftCtrl + LeftAlt + Esc held for 350 ms. It has to be
//! recognised from the raw evdev stream because a grabbed keyboard is invisible
//! to the compositor, so no compositor shortcut can release it.
//!
//! Only Esc is withheld from the remote while the chord is pending. Ctrl and
//! Alt were already forwarded (they are ordinary modifiers); the release path
//! sends their key-ups before ungrab. If the chord is cancelled, the withheld
//! Esc is replayed so a normal Ctrl+Alt+Esc still reaches the remote.

use std::time::{Duration, Instant};

pub const LINUX_KEY_ESC: u16 = 1;
pub const LINUX_KEY_LEFTCTRL: u16 = 29;
pub const LINUX_KEY_LEFTALT: u16 = 56;
pub const HOLD: Duration = Duration::from_millis(350);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChordStep {
    /// Do not forward the physical event to the remote.
    pub consume: bool,
    /// Replay the withheld Esc key-down (a normal Ctrl+Alt+Esc was cancelled).
    pub replay_esc_down: bool,
    /// Replay Esc key-down and key-up (the user released Esc before the hold).
    pub replay_esc_up: bool,
}

#[derive(Debug, Default)]
pub struct ReleaseChord {
    ctrl: bool,
    alt: bool,
    esc: bool,
    /// Esc was withheld, then replayed after a cancellation.
    esc_forwarded: bool,
    started: Option<Instant>,
    completed: bool,
}

/// The focused-window chord that re-enables a paused grab. This one is a tap,
/// not a hold: Ctrl+Alt+Esc key-down re-enables, and the matching key-up is
/// consumed even if Ctrl/Alt were released first.
#[derive(Debug, Default)]
pub struct ResumeChord {
    pending: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapAction {
    /// Not the resume chord; handle normally.
    Forward,
    /// Part of the resume chord; do not send it.
    Consume,
    /// The key-down consumed to resume the grab; do not send it.
    Resume,
}

impl ResumeChord {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn pending(&self) -> bool {
        self.pending
    }

    pub fn reset(&mut self) {
        self.pending = false;
    }

    pub fn on_escape(
        &mut self,
        down: bool,
        ctrl: bool,
        alt: bool,
        repeat: bool,
    ) -> TapAction {
        if down {
            if self.pending {
                return TapAction::Consume;
            }
            if ctrl && alt && !repeat {
                self.pending = true;
                return TapAction::Resume;
            }
            TapAction::Forward
        } else if self.pending {
            self.pending = false;
            TapAction::Consume
        } else {
            TapAction::Forward
        }
    }
}

impl ReleaseChord {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one raw key. `down` is true for press/repeat, false for release.
    pub fn on_key(&mut self, code: u16, down: bool, now: Instant) -> ChordStep {
        if self.completed {
            return ChordStep {
                consume: true,
                ..ChordStep::default()
            };
        }

        match code {
            LINUX_KEY_LEFTCTRL => {
                if down {
                    self.ctrl = true;
                } else {
                    self.ctrl = false;
                    if self.started.is_some() {
                        return self.cancel();
                    }
                }
                self.forward()
            }
            LINUX_KEY_LEFTALT => {
                if down {
                    self.alt = true;
                } else {
                    self.alt = false;
                    if self.started.is_some() {
                        return self.cancel();
                    }
                }
                self.forward()
            }
            LINUX_KEY_ESC => {
                if down {
                    if self.esc {
                        // Auto-repeat while the chord is held.
                        return ChordStep {
                            consume: true,
                            ..ChordStep::default()
                        };
                    }
                    self.esc = true;
                    if self.ctrl && self.alt {
                        self.started = Some(now);
                        self.esc_forwarded = false;
                        return ChordStep {
                            consume: true,
                            ..ChordStep::default()
                        };
                    }
                    self.forward()
                } else {
                    if !self.esc {
                        return self.forward();
                    }
                    self.esc = false;
                    if self.started.is_some() {
                        // Released too early: give the remote a normal press.
                        self.started = None;
                        self.esc_forwarded = true;
                        return ChordStep {
                            consume: true,
                            replay_esc_down: true,
                            replay_esc_up: true,
                        };
                    }
                    if self.esc_forwarded {
                        self.esc_forwarded = false;
                        self.forward()
                    } else {
                        ChordStep {
                            consume: true,
                            ..ChordStep::default()
                        }
                    }
                }
            }
            _ => {
                // Any other key pressed during the hold cancels it. The key
                // itself is still forwarded.
                if down && self.started.is_some() {
                    return self.cancel();
                }
                self.forward()
            }
        }
    }

    /// Check the hold timer. Returns true exactly once when the chord fires.
    pub fn tick(&mut self, now: Instant) -> bool {
        if self.completed {
            return false;
        }
        let Some(started) = self.started else {
            return false;
        };
        if self.ctrl && self.alt && self.esc && now.duration_since(started) >= HOLD {
            self.completed = true;
            self.started = None;
            return true;
        }
        false
    }

    pub fn completed(&self) -> bool {
        self.completed
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    fn forward(&self) -> ChordStep {
        ChordStep::default()
    }

    fn cancel(&mut self) -> ChordStep {
        self.started = None;
        if self.esc && !self.esc_forwarded {
            self.esc_forwarded = true;
            ChordStep {
                replay_esc_down: true,
                ..ChordStep::default()
            }
        } else {
            ChordStep::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    #[test]
    fn a_full_hold_fires_once_after_350ms() {
        let mut chord = ReleaseChord::new();
        let t = at(0);
        assert!(chord.on_key(LINUX_KEY_LEFTCTRL, true, t).consume == false);
        assert!(chord.on_key(LINUX_KEY_LEFTALT, true, t).consume == false);
        let esc = chord.on_key(LINUX_KEY_ESC, true, t);
        assert!(esc.consume, "Esc is withheld while the hold is pending");
        assert!(!chord.tick(t + Duration::from_millis(349)));
        assert!(chord.tick(t + HOLD));
        assert!(chord.completed());
        assert!(!chord.tick(t + HOLD));
        // After completion everything is consumed while the grab winds down.
        assert!(chord.on_key(LINUX_KEY_ESC, false, t + HOLD).consume);
    }

    #[test]
    fn releasing_esc_early_replays_a_normal_press() {
        let mut chord = ReleaseChord::new();
        let t = at(0);
        chord.on_key(LINUX_KEY_LEFTCTRL, true, t);
        chord.on_key(LINUX_KEY_LEFTALT, true, t);
        chord.on_key(LINUX_KEY_ESC, true, t);
        let step = chord.on_key(LINUX_KEY_ESC, false, t + Duration::from_millis(100));
        assert!(step.consume);
        assert!(step.replay_esc_down && step.replay_esc_up);
        assert!(!chord.tick(t + Duration::from_secs(1)));
    }

    #[test]
    fn another_key_cancels_and_replays_esc_down_only() {
        let mut chord = ReleaseChord::new();
        let t = at(0);
        chord.on_key(LINUX_KEY_LEFTCTRL, true, t);
        chord.on_key(LINUX_KEY_LEFTALT, true, t);
        chord.on_key(LINUX_KEY_ESC, true, t);
        let step = chord.on_key(30, true, t + Duration::from_millis(100)); // KEY_A
        assert!(!step.consume, "the other key is still forwarded");
        assert!(step.replay_esc_down);
        assert!(!step.replay_esc_up);
        // Esc is now forwarded, so its release goes through.
        let up = chord.on_key(LINUX_KEY_ESC, false, t + Duration::from_millis(150));
        assert!(!up.consume);
        assert!(!chord.tick(t + Duration::from_secs(1)));
    }

    #[test]
    fn releasing_ctrl_early_cancels() {
        let mut chord = ReleaseChord::new();
        let t = at(0);
        chord.on_key(LINUX_KEY_LEFTCTRL, true, t);
        chord.on_key(LINUX_KEY_LEFTALT, true, t);
        chord.on_key(LINUX_KEY_ESC, true, t);
        let step = chord.on_key(LINUX_KEY_LEFTCTRL, false, t + Duration::from_millis(100));
        assert!(!step.consume);
        assert!(step.replay_esc_down);
        assert!(!chord.tick(t + Duration::from_secs(1)));
    }

    #[test]
    fn ctrl_alt_without_esc_never_fires() {
        let mut chord = ReleaseChord::new();
        let t = at(0);
        chord.on_key(LINUX_KEY_LEFTCTRL, true, t);
        chord.on_key(LINUX_KEY_LEFTALT, true, t);
        assert!(!chord.tick(t + Duration::from_secs(1)));
    }

    #[test]
    fn resume_tap_consumes_down_and_up() {
        let mut tap = ResumeChord::new();
        assert_eq!(tap.on_escape(true, true, true, false), TapAction::Resume);
        assert!(tap.pending());
        // Ctrl/Alt may be released before Escape; the up is still ours.
        assert_eq!(tap.on_escape(false, false, false, false), TapAction::Consume);
        assert!(!tap.pending());
    }

    #[test]
    fn escape_without_ctrl_alt_is_forwarded() {
        let mut tap = ResumeChord::new();
        assert_eq!(tap.on_escape(true, false, false, false), TapAction::Forward);
        assert_eq!(tap.on_escape(false, false, false, false), TapAction::Forward);
    }

    #[test]
    fn a_held_escape_repeat_is_consumed_after_resume() {
        let mut tap = ResumeChord::new();
        assert_eq!(tap.on_escape(true, true, true, false), TapAction::Resume);
        assert_eq!(tap.on_escape(true, true, true, true), TapAction::Consume);
        assert_eq!(tap.on_escape(false, true, true, false), TapAction::Consume);
    }
}
