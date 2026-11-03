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
    Scroll {
        dx: f32,
        dy: f32,
        phase: Phase,
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
                            dx,
                            dy,
                            phase: Phase::Changed,
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
                    });
                }
            } else {
                out.push(Gesture::Scroll {
                    dx: 0.0,
                    dy: 0.0,
                    phase: Phase::Began,
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
