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
/// or two lines, i.e. look broken.
pub const SCROLL_POINTS_PER_LINE: f32 = 12.0;

/// Inertia after the fingers lift.
///
/// The server truncates every scroll delta to whole lines and never generates
/// momentum itself, so the glide is a decaying *rate* of one-line steps: a
/// decaying amplitude would round to zero.
pub const MOMENTUM_START_HZ: f32 = 90.0;
/// Each step lengthens the interval by this factor.
pub const MOMENTUM_RATE_DECAY: f32 = 0.72;
/// Stop when the step rate falls below this.
pub const MOMENTUM_MIN_HZ: f32 = 8.0;
/// Hard stop, so a flick cannot glide forever.
pub const MOMENTUM_MAX: std::time::Duration = std::time::Duration::from_millis(1100);
/// Below this finger speed (points/second) a release does not glide.
pub const MOMENTUM_MIN_SPEED: f32 = 120.0;

/// Drives the post-release glide. Portable: any backend that knows the last
/// scroll velocity can start it, and the GUI ticks it.
#[derive(Debug)]
pub struct Momentum {
    dir: (i32, i32),
    active: bool,
    next_at: std::time::Instant,
    interval: std::time::Duration,
    deadline: std::time::Instant,
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
            dir: (0, 0),
            active: false,
            next_at: now,
            interval: std::time::Duration::from_millis(11),
            deadline: now,
        }
    }

    /// Start a glide from the last scroll velocity (points per second).
    /// Returns false when the release was too slow to deserve one.
    pub fn start(&mut self, vx: f32, vy: f32, now: std::time::Instant) -> bool {
        let speed = (vx * vx + vy * vy).sqrt();
        if speed < MOMENTUM_MIN_SPEED {
            self.active = false;
            return false;
        }
        self.dir = (
            if vx.abs() > speed * 0.2 { vx.signum() as i32 } else { 0 },
            if vy.abs() > speed * 0.2 { vy.signum() as i32 } else { 0 },
        );
        self.active = true;
        self.interval = std::time::Duration::from_secs_f32(1.0 / MOMENTUM_START_HZ);
        self.next_at = now + self.interval;
        self.deadline = now + MOMENTUM_MAX;
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

    /// Emit the next glide step if one is due: exactly one line, in the
    /// direction of the flick, at the current (decaying) rate.
    pub fn tick(&mut self, now: std::time::Instant) -> Option<Gesture> {
        if !self.active || now < self.next_at {
            return None;
        }
        if now >= self.deadline {
            self.active = false;
            return None;
        }
        self.interval = std::time::Duration::from_secs_f32(
            self.interval.as_secs_f32() / MOMENTUM_RATE_DECAY,
        );
        self.next_at = now + self.interval;
        if 1.0 / self.interval.as_secs_f32() < MOMENTUM_MIN_HZ {
            self.active = false;
        }
        Some(Gesture::Scroll {
            dx: self.dir.0 as f32 * SCROLL_POINTS_PER_LINE,
            dy: self.dir.1 as f32 * SCROLL_POINTS_PER_LINE,
            phase: Phase::Changed,
            precise: true,
            momentum: Some(Phase::Changed),
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
    fn glide_is_a_decaying_rate_of_one_line_steps() {
        let mut m = Momentum::new();
        let t0 = Instant::now();
        assert!(m.start(0.0, -600.0, t0));
        let mut steps = 0;
        let mut times = Vec::new();
        let mut now = t0;
        // Advance in 1 ms slices for 2 s; count the steps and their spacing.
        for _ in 0..2000 {
            now += Duration::from_millis(1);
            if let Some(g) = m.tick(now) {
                match g {
                    Gesture::Scroll { dx, dy, phase, momentum, .. } => {
                        assert_eq!(dx, 0.0);
                        assert_eq!(dy, -SCROLL_POINTS_PER_LINE);
                        assert_eq!(phase, Phase::Changed);
                        assert_eq!(momentum, Some(Phase::Changed));
                    }
                    _ => panic!("momentum must emit scroll"),
                }
                times.push(now);
                steps += 1;
            }
        }
        assert!(steps >= 5, "expected a glide, got {steps} steps");
        assert!(!m.active(), "the glide must stop");
        // Each interval is longer than the last (decaying rate).
        let gaps: Vec<_> = times.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(gaps.windows(2).all(|w| w[1] >= w[0]), "rate must decay: {gaps:?}");
    }

    #[test]
    fn a_slow_release_does_not_glide() {
        let mut m = Momentum::new();
        assert!(!m.start(10.0, 10.0, Instant::now()));
        assert!(m.tick(Instant::now() + Duration::from_secs(1)).is_none());
    }
}
