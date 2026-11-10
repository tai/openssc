//! Momentum-phase classification for scroll events.
//!
//! winit maps `NSEvent.momentumPhase` onto the same `TouchPhase` sequence as a
//! real touch (momentum Began -> Started), and gives it priority over the
//! touch phase, so a gesture that continues after the fingers lift is
//! indistinguishable from a new touch in the winit event. A local NSEvent
//! monitor runs before the event is dispatched, so it can read the real
//! `momentumPhase` and record one classification per event; the GUI pops it in
//! its `MouseWheel` handler.
//!
//! The queue is portable and the only part that needs testing. On platforms
//! without the monitor `start()` returns false and nothing is ever queued, so
//! `take()` returns `None` — the same as an unclassified event.

use std::collections::VecDeque;
use std::sync::Mutex;

/// One entry per scroll event, in dispatch order: `true` = momentum.
static QUEUE: Mutex<VecDeque<bool>> = Mutex::new(VecDeque::new());

/// Install the local monitor. Returns false when the API returns nil (the
/// caller then falls back to forwarding every event).
#[cfg(target_os = "macos")]
pub fn start() -> bool {
    use block2::RcBlock;
    use objc2_app_kit::{NSEvent, NSEventMask, NSEventPhase};
    use std::ptr::NonNull;

    let handler = RcBlock::new(|event: NonNull<NSEvent>| -> *mut NSEvent {
        let momentum = unsafe { event.as_ref().momentumPhase() } != NSEventPhase::None;
        if let Ok(mut queue) = QUEUE.lock() {
            // Bounded: a stuck consumer must not grow this without limit.
            if queue.len() >= 256 {
                queue.pop_front();
            }
            queue.push_back(momentum);
        }
        event.as_ptr()
    });
    let monitor = unsafe {
        NSEvent::addLocalMonitorForEventsMatchingMask_handler(
            NSEventMask::ScrollWheel,
            &handler,
        )
    };
    match monitor {
        Some(monitor) => {
            // The monitor lives for the process; removing it would mean
            // keeping the Retained alive across threads.
            std::mem::forget(monitor);
            true
        }
        None => false,
    }
}

/// No momentum monitor on this platform: every scroll event is unclassified
/// and the GUI forwards it.
#[cfg(not(target_os = "macos"))]
pub fn start() -> bool {
    false
}

/// Pop the classification for the event winit is about to deliver.
pub fn take() -> Option<bool> {
    QUEUE.lock().ok().and_then(|mut queue| queue.pop_front())
}

/// How many classifications are queued (for the desync counter).
pub fn pending() -> usize {
    QUEUE.lock().map(|queue| queue.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_is_fifo_and_bounded() {
        if let Ok(mut q) = QUEUE.lock() {
            q.clear();
            q.push_back(false);
            q.push_back(true);
        }
        assert_eq!(take(), Some(false));
        assert_eq!(take(), Some(true));
        assert_eq!(take(), None);
    }
}
