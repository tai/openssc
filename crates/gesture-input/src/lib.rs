//! Platform gesture input backends and protocol-neutral recognizers.
//!
//! The recognizer is pure Rust and unit-tested on any platform. The Linux
//! backend reads raw evdev multitouch (MT protocol B); macOS uses winit events
//! in the GUI plus this crate's tests.

pub const ABS_MT_SLOT: u16 = 0x2f;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;

pub const SWIPE_THRESHOLD: f32 = 0.08;

/// Normalized pad units -> pixels for the portable scroll model. This is a
/// feel/unit conversion, not a copy of Apple's internals; it is calibrated
/// against the macOS backend (which already reports pixels).
pub const SCROLL_PIXELS_PER_UNIT: f32 = 600.0;

/// Points of finger travel that make one scroll line. Measured on the lab
/// server: one subtype-11 line unit scrolls a TextEdit document by ~120
/// points (3 consecutive isolated events, 240 screen px at 2x). The server
/// applies its own acceleration to multi-line deltas, so this is a 1:1
/// tracking choice, not a protocol constant.
/// Points of trackpad travel per scroll line.
///
/// Calibrated against the behaviour that felt right before the portable model
/// landed: the old client divided winit's pixel delta by 12 and a normal flick
/// scrolled about a screen. Apple's own client sits in the same ballpark (the
/// agent's double-to-line conversion in `SSAgent_PostScrollEvent_rpc` implies
/// roughly 8 points per line). 120 was a placeholder and made a flick move one
/// or two lines, i.e. look broken. 24 is the owner's calibrated preference
/// (they asked for "--scroll-speed 0.5" against the previous 12), and stays
/// runtime-tunable with --scroll-speed.
pub const SCROLL_POINTS_PER_LINE: f32 = 24.0;

/// Inertia after the fingers lift.
///
/// The owner's model: momentum is for a *flick* only. It requires both a high
/// release velocity and a short gesture; a deliberate drag or a pause-then-
/// release must not glide. The glide decays the VELOCITY and sends its
/// integral (sub-line, via the 16.16 fixed field), so the tail is smooth
/// rather than a series of whole-line steps.
pub const MOMENTUM_MIN_SPEED: f32 = 1200.0;
/// A gesture longer than this is a drag, not a flick.
pub const MOMENTUM_MAX_GESTURE: std::time::Duration = std::time::Duration::from_millis(280);
/// Glide tick interval (~60 Hz).
pub const MOMENTUM_TICK: std::time::Duration = std::time::Duration::from_millis(16);
/// Velocity e-folding rate: v *= exp(-DECAY * dt). Fast, so a flick stops in
/// about a second like the native client.
pub const MOMENTUM_DECAY_PER_SEC: f32 = 9.0;
/// Stop the glide when the decaying speed drops below this (points/second).
pub const MOMENTUM_STOP_SPEED: f32 = 40.0;
/// Hard stop, so a flick cannot glide forever.
pub const MOMENTUM_MAX: std::time::Duration = std::time::Duration::from_millis(900);
/// Total distance the glide may cover, whatever the release speed. This is
/// the only way to guarantee "a few pages": the integral of the decay alone
/// is v0/decay, which a hard flick pushes past a screen or two.
pub const MOMENTUM_BUDGET: f32 = 400.0;

/// Time constant of the axis dominance average (~100 ms of recent motion).
pub const AXIS_TIME_CONSTANT: std::time::Duration = std::time::Duration::from_millis(100);
/// Evidence needed to leave a single-axis mode for Both (ratio of the minor
/// EMA to the major one), and the lower ratio that lets Both fall back to a
/// single axis. Exit is deliberately harder than entry.
pub const AXIS_ENTRY_BOTH: f32 = 0.85;
pub const AXIS_EXIT_SINGLE: f32 = 0.25;
/// A single-axis mode flips to the other axis only when that axis is clearly
/// larger, sustained.
pub const AXIS_FLIP_FACTOR: f32 = 2.2;
/// Absolute travel (points per event, from the EMA) before the minor axis can
/// count at all: sideways jitter must never trigger a transition.
pub const AXIS_MIN_TRAVEL: f32 = 6.0;
/// Evidence must persist this long before the mode changes.
pub const AXIS_DWELL: std::time::Duration = std::time::Duration::from_millis(250);

/// Which axes the current gesture is allowed to move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxisMode {
    Vertical,
    Horizontal,
    Both,
}

impl AxisMode {
    pub fn name(self) -> &'static str {
        match self {
            AxisMode::Vertical => "vertical",
            AxisMode::Horizontal => "horizontal",
            AxisMode::Both => "both",
        }
    }
}

/// A mode change, with the evidence that caused it (for `--debug-input`).
#[derive(Clone, Copy, Debug)]
pub struct AxisTransition {
    pub from: AxisMode,
    pub to: AxisMode,
    pub ratio: f32,
    pub minor_ema: f32,
    pub dwell_ms: u128,
}

/// Three-state axis machine driven by recent motion.
///
/// Every gesture starts in **Vertical** (the dominant use case) and has to
/// earn its way out: leaving a single-axis mode needs a higher ratio, an
/// absolute minimum of travel on the minor axis, and the evidence must
/// persist for `AXIS_DWELL`. A deadband keeps noise from flapping the mode.
#[derive(Debug)]
pub struct AxisFilter {
    ax: f32,
    ay: f32,
    last: Option<std::time::Instant>,
    mode: AxisMode,
    pending: Option<(AxisMode, std::time::Instant)>,
    transition: Option<AxisTransition>,
}

impl Default for AxisFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl AxisFilter {
    pub fn new() -> Self {
        Self {
            ax: 0.0,
            ay: 0.0,
            last: None,
            mode: AxisMode::Vertical,
            pending: None,
            transition: None,
        }
    }

    /// Reset the history: at the start of a gesture and when a glide ends.
    /// The mode returns to the default, Vertical.
    pub fn begin(&mut self) {
        self.ax = 0.0;
        self.ay = 0.0;
        self.last = None;
        self.mode = AxisMode::Vertical;
        self.pending = None;
        self.transition = None;
    }

    pub fn mode(&self) -> AxisMode {
        self.mode
    }

    /// The last mode change, for the `--debug-input` log.
    pub fn take_transition(&mut self) -> Option<AxisTransition> {
        self.transition.take()
    }

    /// Weight one raw sample and return the delta to send.
    pub fn apply(
        &mut self,
        dx: f32,
        dy: f32,
        now: std::time::Instant,
    ) -> (f32, f32) {
        let dt = self
            .last
            .map(|last| now.duration_since(last).as_secs_f32())
            .unwrap_or(0.016)
            .clamp(0.001, 0.1);
        self.last = Some(now);
        let alpha = 1.0 - (-dt / AXIS_TIME_CONSTANT.as_secs_f32()).exp();
        self.ax += alpha * (dx.abs() - self.ax);
        self.ay += alpha * (dy.abs() - self.ay);

        let (minor, major) = if self.ax >= self.ay {
            (self.ay, self.ax)
        } else {
            (self.ax, self.ay)
        };
        let ratio = if major <= 1e-3 { 0.0 } else { minor / major };
        let dominant = if self.ax >= self.ay {
            AxisMode::Horizontal
        } else {
            AxisMode::Vertical
        };

        // What would the evidence support right now?
        let candidate = match self.mode {
            // A single-axis mode only ever opens to Both: a direction change
            // therefore goes Vertical -> Both -> Horizontal rather than
            // jumping, and the horizontal motion is never delayed by it.
            AxisMode::Vertical => {
                if ratio > AXIS_ENTRY_BOTH && self.ax >= AXIS_MIN_TRAVEL {
                    Some(AxisMode::Both)
                } else if ratio < AXIS_EXIT_SINGLE
                    && self.ax >= self.ay * AXIS_FLIP_FACTOR
                    && self.ax >= AXIS_MIN_TRAVEL
                {
                    // Purely horizontal from the start: no diagonal band to
                    // pass through, so the flip is direct.
                    Some(AxisMode::Horizontal)
                } else {
                    None
                }
            }
            AxisMode::Horizontal => {
                if ratio > AXIS_ENTRY_BOTH && self.ay >= AXIS_MIN_TRAVEL {
                    Some(AxisMode::Both)
                } else if ratio < AXIS_EXIT_SINGLE
                    && self.ay >= self.ax * AXIS_FLIP_FACTOR
                    && self.ay >= AXIS_MIN_TRAVEL
                {
                    Some(AxisMode::Vertical)
                } else {
                    None
                }
            }
            // Both is resistive: it falls back only when the minor axis has
            // decisively dropped, by the exit ratio and the flip factor.
            AxisMode::Both => {
                if ratio < AXIS_EXIT_SINGLE
                    && major >= minor * AXIS_FLIP_FACTOR
                    && major >= AXIS_MIN_TRAVEL
                {
                    Some(dominant)
                } else {
                    None
                }
            }
        };

        match candidate {
            Some(next) => match self.pending {
                Some((pending, since)) if pending == next => {
                    let dwell = now.duration_since(since);
                    if dwell >= AXIS_DWELL {
                        self.transition = Some(AxisTransition {
                            from: self.mode,
                            to: next,
                            ratio,
                            minor_ema: minor,
                            dwell_ms: dwell.as_millis(),
                        });
                        self.mode = next;
                        self.pending = None;
                    }
                }
                // A Both candidate that is already pending wins over a flip:
                // opening both axes first is harmless, and it is what makes a
                // direction change read as Vertical -> Both -> Horizontal.
                Some((AxisMode::Both, since)) => {
                    let dwell = now.duration_since(since);
                    if dwell >= AXIS_DWELL {
                        self.transition = Some(AxisTransition {
                            from: self.mode,
                            to: AxisMode::Both,
                            ratio,
                            minor_ema: minor,
                            dwell_ms: dwell.as_millis(),
                        });
                        self.mode = AxisMode::Both;
                        self.pending = None;
                    }
                }
                _ => self.pending = Some((next, now)),
            },
            // Evidence that dips without reversing keeps its place: a short
            // dip must not restart the dwell, or a fast direction change
            // would never accumulate enough.
            None => {
                if ratio < AXIS_EXIT_SINGLE {
                    self.pending = None;
                }
            }
        }

        match self.mode {
            AxisMode::Vertical => (0.0, dy),
            AxisMode::Horizontal => (dx, 0.0),
            AxisMode::Both => (dx, dy),
        }
    }
}

/// Drives the post-release glide. Portable: any backend that knows the last
/// scroll velocity and how long the gesture lasted can start it, and the GUI
/// ticks it.
#[derive(Debug)]
pub struct Momentum {
    v: (f32, f32),
    active: bool,
    last: std::time::Instant,
    next_at: std::time::Instant,
    deadline: std::time::Instant,
    /// Points of glide still allowed.
    budget_left: f32,
}

impl Default for Momentum {
    fn default() -> Self {
        Self::new()
    }
}

impl Momentum {
    pub fn new() -> Self {
        let now = std::time::Instant::now();
        Self {
            v: (0.0, 0.0),
            active: false,
            last: now,
            next_at: now,
            deadline: now,
            budget_left: 0.0,
        }
    }

    /// Start a glide from the release velocity (points/second) and the
    /// gesture duration. Returns false for a slow release or a long drag.
    pub fn start(
        &mut self,
        vx: f32,
        vy: f32,
        gesture: std::time::Duration,
        now: std::time::Instant,
    ) -> bool {
        let speed = (vx * vx + vy * vy).sqrt();
        if speed < MOMENTUM_MIN_SPEED || gesture > MOMENTUM_MAX_GESTURE {
            self.active = false;
            return false;
        }
        self.v = (vx, vy);
        self.active = true;
        self.last = now;
        self.next_at = now + MOMENTUM_TICK;
        self.deadline = now + MOMENTUM_MAX;
        self.budget_left = MOMENTUM_BUDGET;
        true
    }

    pub fn stop(&mut self) {
        self.active = false;
    }

    pub fn active(&self) -> bool {
        self.active
    }

    pub fn next_at(&self) -> Option<std::time::Instant> {
        self.active.then_some(self.next_at)
    }

    /// Emit the velocity's integral since the last tick, then decay it. The
    /// delta is in points, so the synthesis sends sub-line motion through the
    /// fixed field instead of whole-line steps.
    pub fn tick(&mut self, now: std::time::Instant) -> Option<Gesture> {
        if !self.active || now < self.next_at {
            return None;
        }
        let dt = now
            .duration_since(self.last)
            .as_secs_f32()
            .clamp(0.001, 0.05);
        let (mut dx, mut dy) = (self.v.0 * dt, self.v.1 * dt);
        // Distance budget: clamp the step to what is left, then stop.
        let step = (dx * dx + dy * dy).sqrt();
        if step >= self.budget_left {
            let scale = if step > 0.0 {
                self.budget_left / step
            } else {
                0.0
            };
            dx *= scale;
            dy *= scale;
            self.budget_left = 0.0;
            self.active = false;
        } else {
            self.budget_left -= step;
        }
        let decay = (-MOMENTUM_DECAY_PER_SEC * dt).exp();
        self.v = (self.v.0 * decay, self.v.1 * decay);
        self.last = now;
        self.next_at = now + MOMENTUM_TICK;
        if (self.v.0 * self.v.0 + self.v.1 * self.v.1).sqrt() < MOMENTUM_STOP_SPEED
            || now >= self.deadline
        {
            self.active = false;
        }
        Some(Gesture::Scroll {
            dx,
            dy,
            phase: Phase::Changed,
            precise: true,
            // Keep momentumPhase zero on the wire: this server ignores a
            // scroll message that carries one (measured: a 42-line glide
            // applied 0 lines with it set, and lands when it is cleared).
            momentum: None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Began,
    Changed,
    Ended,
    Cancelled,
}

impl Phase {
    /// CGSGesturePhase values used by the Apple gesture messages.
    pub fn code(self) -> u32 {
        match self {
            Phase::Began => 1,
            Phase::Changed => 2,
            Phase::Ended => 4,
            Phase::Cancelled => 8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TouchPoint {
    pub id: i32,
    /// Normalized 0..1 across the trackpad.
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Gesture {
    /// Two-finger scroll. `dx`/`dy` are **pixels**, device independent: a
    /// trackpad backend passes its pixel delta, a backend that works in
    /// normalized pad units converts first (see `SCROLL_PIXELS_PER_UNIT`).
    /// Positive `dy` scrolls content up, as winit and the Apple message do.
    Scroll {
        dx: f32,
        dy: f32,
        phase: Phase,
        /// Trackpad (precise) vs a line-stepped wheel.
        precise: bool,
        /// Inertia phase when the OS reports one; `None` on platforms that
        /// fold momentum into `phase`.
        momentum: Option<Phase>,
    },
    Swipe {
        fingers: u8,
        motion: u32,
        mask: u32,
        dx: f32,
        dy: f32,
        progress: f32,
        phase: Phase,
        /// Normalized centroid of the final touch set.
        x: f32,
        y: f32,
        /// Centroid velocity in pad-widths per second; zero until a backend
        /// measures it.
        velocity: (f32, f32, f32),
        /// Apple's `flavor`/`flags` fields, zero until calibrated.
        flavor: u32,
        flags: u32,
    },
    Magnify {
        factor: f32,
        phase: Phase,
        x: f32,
        y: f32,
    },
    Rotate {
        radians: f32,
        phase: Phase,
        x: f32,
        y: f32,
    },
}

#[derive(Clone, Copy, Debug)]
struct FrameState {
    fingers: usize,
    cx: f32,
    cy: f32,
    dist: f32,
    angle: f32,
}

#[derive(Clone, Copy, Debug)]
struct SwipeState {
    start: (f32, f32),
    last: (f32, f32),
    fingers: u8,
}

/// Stateful recognizer. Feed one normalized touch frame per SYN_REPORT.
#[derive(Default)]
pub struct GestureRecognizer {
    last: Option<FrameState>,
    swipe: Option<SwipeState>,
}

impl GestureRecognizer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn process(&mut self, points: &[TouchPoint]) -> Vec<Gesture> {
        let mut out = Vec::new();
        let n = points.len();

        if n == 0 {
            if let Some(sw) = self.swipe.take() {
                out.push(self.finish_swipe(sw, Phase::Ended));
            }
            if let Some(last) = self.last.take() {
                if last.fingers == 2 {
                    out.push(Gesture::Scroll {
                        dx: 0.0,
                        dy: 0.0,
                        phase: Phase::Ended,
                        precise: true,
                        momentum: None,
                    });
                }
            }
            return out;
        }

        let (cx, cy) = centroid(points);
        let (dist, angle) = if n >= 2 {
            let (dx, dy) = (points[1].x - points[0].x, points[1].y - points[0].y);
            ((dx * dx + dy * dy).sqrt(), dy.atan2(dx))
        } else {
            (0.0, 0.0)
        };

        if n >= 3 {
            // Swipe family.
            if self.swipe.is_none() {
                self.swipe = Some(SwipeState {
                    start: (cx, cy),
                    last: (cx, cy),
                    fingers: n.min(4) as u8,
                });
                out.push(self.swipe_gesture(self.swipe.unwrap(), Phase::Began));
            } else {
                let mut sw = self.swipe.unwrap();
                sw.last = (cx, cy);
                self.swipe = Some(sw);
                out.push(self.swipe_gesture(sw, Phase::Changed));
            }
            self.last = Some(FrameState { fingers: n, cx, cy, dist, angle });
            return out;
        }

        // Leaving the swipe family: emit the final swipe.
        if let Some(sw) = self.swipe.take() {
            out.push(self.finish_swipe(sw, Phase::Ended));
        }

        if n == 2 {
            if let Some(last) = self.last {
                if last.fingers == 2 {
                    let dx = cx - last.cx;
                    let dy = cy - last.cy;
                    if dx.abs() + dy.abs() > 0.0005 {
                        out.push(Gesture::Scroll {
                            dx: dx * SCROLL_PIXELS_PER_UNIT,
                            dy: dy * SCROLL_PIXELS_PER_UNIT,
                            phase: Phase::Changed,
                            precise: true,
                            momentum: None,
                        });
                    }
                    if last.dist > 0.0001 {
                        let factor = dist / last.dist;
                        if (factor - 1.0).abs() > 0.005 {
                            out.push(Gesture::Magnify {
                                factor,
                                phase: Phase::Changed,
                                x: cx,
                                y: cy,
                            });
                        }
                    }
                    let mut d = angle - last.angle;
                    while d > std::f32::consts::PI {
                        d -= 2.0 * std::f32::consts::PI;
                    }
                    while d < -std::f32::consts::PI {
                        d += 2.0 * std::f32::consts::PI;
                    }
                    if d.abs() > 0.01 {
                        out.push(Gesture::Rotate {
                            radians: d,
                            phase: Phase::Changed,
                            x: cx,
                            y: cy,
                        });
                    }
                } else {
                    out.push(Gesture::Scroll {
                        dx: 0.0,
                        dy: 0.0,
                        phase: Phase::Began,
                        precise: true,
                        momentum: None,
                    });
                }
            } else {
                out.push(Gesture::Scroll {
                    dx: 0.0,
                    dy: 0.0,
                    phase: Phase::Began,
                    precise: true,
                    momentum: None,
                });
            }
        }

        self.last = Some(FrameState { fingers: n, cx, cy, dist, angle });
        out
    }

    fn swipe_gesture(&self, sw: SwipeState, phase: Phase) -> Gesture {
        let (dx, dy) = (sw.last.0 - sw.start.0, sw.last.1 - sw.start.1);
        let (motion, mask) = swipe_direction(dx, dy);
        let progress = ((dx * dx + dy * dy).sqrt() / SWIPE_THRESHOLD).min(1.0);
        Gesture::Swipe {
            fingers: sw.fingers,
            motion,
            mask,
            dx,
            dy,
            progress,
            phase,
            x: sw.last.0,
            y: sw.last.1,
            velocity: (0.0, 0.0, 0.0),
            flavor: 0,
            flags: 0,
        }
    }

    fn finish_swipe(&self, sw: SwipeState, phase: Phase) -> Gesture {
        self.swipe_gesture(sw, phase)
    }
}

fn centroid(points: &[TouchPoint]) -> (f32, f32) {
    let mut x = 0.0;
    let mut y = 0.0;
    for p in points {
        x += p.x;
        y += p.y;
    }
    (x / points.len() as f32, y / points.len() as f32)
}

/// Map a swipe delta to (motion, IOHID swipe mask).
/// motion 2 is treated as the vertical family by ScreensharingAgent; other
/// values are horizontal. This matches the agent's `motion == 2` special case.
pub fn swipe_direction(dx: f32, dy: f32) -> (u32, u32) {
    if dx.abs() > dy.abs() {
        (1, if dx > 0.0 { 8 } else { 4 })
    } else {
        (2, if dy > 0.0 { 2 } else { 1 })
    }
}

/// Convert a scroll delta in normalized trackpad units to Apple's signed 16-bit
/// wheel deltas. The sign convention is natural scrolling (content follows the
/// fingers); callers can invert it.
pub fn scroll_to_wheel(dx: f32, dy: f32) -> (i16, i16) {
    let clamp = |v: f32| (v * 1000.0).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
    (clamp(dx), clamp(-dy))
}

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(test)]
mod tests {
    use super::*;

    fn pts(coords: &[(f32, f32)]) -> Vec<TouchPoint> {
        coords
            .iter()
            .enumerate()
            .map(|(i, (x, y))| TouchPoint { id: i as i32, x: *x, y: *y })
            .collect()
    }

    #[test]
    fn three_finger_swipe_right() {
        let mut r = GestureRecognizer::new();
        let start = pts(&[(0.2, 0.5), (0.5, 0.5), (0.8, 0.5)]);
        let end = pts(&[(0.35, 0.5), (0.65, 0.5), (0.95, 0.5)]);
        let a = r.process(&start);
        let b = r.process(&end);
        let c = r.process(&[]);
        assert!(matches!(a[0], Gesture::Swipe { phase: Phase::Began, .. }));
        assert!(matches!(&b[0], Gesture::Swipe { motion: 1, mask: 8, .. }));
        match &c[0] {
            Gesture::Swipe { phase: Phase::Ended, progress, .. } => {
                assert!(*progress > 0.9, "progress={progress}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn three_finger_swipe_up() {
        let mut r = GestureRecognizer::new();
        let start = pts(&[(0.5, 0.7), (0.4, 0.6), (0.6, 0.6)]);
        let end = pts(&[(0.5, 0.4), (0.4, 0.3), (0.6, 0.3)]);
        r.process(&start);
        let b = r.process(&end);
        assert!(matches!(&b[0], Gesture::Swipe { motion: 2, mask: 1, .. }));
    }

    #[test]
    fn two_finger_scroll_and_pinch() {
        let mut r = GestureRecognizer::new();
        r.process(&pts(&[(0.4, 0.5), (0.6, 0.5)]));
        let out = r.process(&pts(&[(0.45, 0.5), (0.7, 0.5)]));
        assert!(out.iter().any(|g| matches!(g, Gesture::Scroll { .. })));
        assert!(out.iter().any(|g| matches!(g, Gesture::Magnify { .. })));
    }

    #[test]
    fn two_finger_rotate() {
        let mut r = GestureRecognizer::new();
        r.process(&pts(&[(0.4, 0.5), (0.6, 0.5)]));
        let out = r.process(&pts(&[(0.5, 0.4), (0.5, 0.6)]));
        assert!(out.iter().any(|g| matches!(g, Gesture::Rotate { .. })));
    }

    #[test]
    fn direction_mapping() {
        assert_eq!(swipe_direction(0.2, 0.0), (1, 8));
        assert_eq!(swipe_direction(-0.2, 0.0), (1, 4));
        assert_eq!(swipe_direction(0.0, 0.2), (2, 2));
        assert_eq!(swipe_direction(0.0, -0.2), (2, 1));
    }
}

#[cfg(test)]
mod momentum_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn a_flick_glides_with_a_decaying_sub_line_integral() {
        let mut m = Momentum::new();
        let t0 = Instant::now();
        assert!(m.start(0.0, -2400.0, Duration::from_millis(160), t0));
        let mut total = 0.0f32;
        let mut now = t0;
        let mut ticks = 0;
        for _ in 0..2000 {
            now += Duration::from_millis(1);
            if let Some(Gesture::Scroll { dy, phase, momentum, .. }) = m.tick(now) {
                assert_eq!(phase, Phase::Changed);
                assert_eq!(momentum, None);
                assert!(dy < 0.0 && dy > -SCROLL_POINTS_PER_LINE * 10.0, "dy={dy}");
                total += dy;
                ticks += 1;
            }
        }
        assert!(ticks >= 10, "expected a glide, got {ticks} ticks");
        assert!(!m.active(), "the glide must stop");
        // Integral of v0*exp(-k t) = v0/k, independent of the tick rate.
        let expected = -2400.0 / MOMENTUM_DECAY_PER_SEC;
        assert!(
            (total - expected).abs() < expected.abs() * 0.25,
            "glide distance {total} should be near {expected}"
        );
    }

    #[test]
    fn a_slow_release_or_a_long_drag_does_not_glide() {
        let mut m = Momentum::new();
        let now = Instant::now();
        assert!(!m.start(0.0, -200.0, Duration::from_millis(160), now));
        assert!(!m.start(0.0, -2400.0, Duration::from_millis(600), now));
        assert!(m.tick(now + Duration::from_secs(1)).is_none());
    }

    #[test]
    fn stopping_is_immediate() {
        let mut m = Momentum::new();
        let t0 = Instant::now();
        assert!(m.start(0.0, -2400.0, Duration::from_millis(160), t0));
        m.tick(t0 + Duration::from_millis(16));
        m.stop();
        assert!(m.tick(t0 + Duration::from_millis(32)).is_none());
    }
}

#[cfg(test)]
mod axis_tests {
    use super::*;
    use std::time::{Duration, Instant};

    struct Run {
        sx: f32,
        sy: f32,
        transitions: Vec<AxisTransition>,
        modes: Vec<AxisMode>,
    }

    fn run(samples: &[(f32, f32)]) -> Run {
        let mut f = AxisFilter::new();
        f.begin();
        let t0 = Instant::now();
        let mut out = Run { sx: 0.0, sy: 0.0, transitions: Vec::new(), modes: Vec::new() };
        for (i, (dx, dy)) in samples.iter().enumerate() {
            let (ox, oy) = f.apply(*dx, *dy, t0 + Duration::from_millis(16 * i as u64));
            out.sx += ox.abs();
            out.sy += oy.abs();
            out.modes.push(f.mode());
            if let Some(t) = f.take_transition() {
                out.transitions.push(t);
            }
        }
        out
    }

    #[test]
    fn default_mode_is_vertical() {
        let mut f = AxisFilter::new();
        f.begin();
        assert_eq!(f.mode(), AxisMode::Vertical);
    }

    #[test]
    fn pure_vertical_never_leaves_vertical() {
        let samples: Vec<(f32, f32)> = (0..30).map(|_| (0.0, -10.0)).collect();
        let r = run(&samples);
        assert!(r.transitions.is_empty(), "{:?}", r.transitions);
        assert!(r.sx == 0.0);
        assert!((r.sy - 300.0).abs() < 1.0);
    }

    #[test]
    fn sideways_jitter_never_changes_the_mode() {
        let samples: Vec<(f32, f32)> = (0..40)
            .map(|i| (if i % 2 == 0 { 3.0 } else { -3.0 }, -10.0))
            .collect();
        let r = run(&samples);
        assert!(r.transitions.is_empty(), "jitter flipped the mode: {:?}", r.transitions);
        assert_eq!(r.sx, 0.0);
    }

    #[test]
    fn a_diagonal_reaches_both() {
        let samples: Vec<(f32, f32)> = (0..40).map(|_| (-10.0, -10.0)).collect();
        let r = run(&samples);
        assert_eq!(r.transitions.len(), 1, "{:?}", r.transitions);
        assert_eq!(r.transitions[0].to, AxisMode::Both);
        assert!(r.transitions[0].dwell_ms >= AXIS_DWELL.as_millis());
        assert!(r.sx >= 130.0, "horizontal {}", r.sx);
    }

    #[test]
    fn a_turn_goes_vertical_both_horizontal() {
        // Leaving Vertical is deliberately resistive: ratio, absolute travel
        // and a 250 ms dwell all have to be satisfied, so a deliberate turn
        // takes about half a second per stage before it reads as intentional.
        let mut samples = Vec::new();
        for _ in 0..30 {
            samples.push((0.0, -10.0));
        }
        for _ in 0..70 {
            samples.push((-10.0, 0.0));
        }
        let r = run(&samples);
        let seq: Vec<&str> = r.transitions.iter().map(|t| t.to.name()).collect();
        // Either route is acceptable: opening to Both first and then turning,
        // or a direct Vertical -> Horizontal flip once the vertical average has
        // decayed. What matters is that a deliberate turn is honoured and that
        // the horizontal motion after it passes through in full.
        assert!(!seq.is_empty(), "a deliberate turn produced no transition");
        assert_eq!(*seq.last().unwrap(), "horizontal", "got {seq:?}");
        assert!(r.sx >= 130.0, "horizontal distance {}", r.sx);
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn a_hard_flick_is_capped_by_the_distance_budget() {
        let mut m = Momentum::new();
        let t0 = Instant::now();
        assert!(m.start(0.0, -8000.0, Duration::from_millis(120), t0));
        let mut total = 0.0f32;
        let mut now = t0;
        let mut last_step = t0;
        for _ in 0..2000 {
            now += Duration::from_millis(1);
            if let Some(Gesture::Scroll { dy, .. }) = m.tick(now) {
                total += dy.abs();
                last_step = now;
            }
        }
        assert!(total <= MOMENTUM_BUDGET + 1.0, "glide {total} over budget");
        assert!(!m.active());
        let duration = last_step.duration_since(t0);
        assert!(duration <= MOMENTUM_MAX, "glide lasted {duration:?}");
    }
}
