//! Platform seam: the only place in `ss-gui` that knows which OS it runs on.
//!
//! macOS gets the real implementations — a `CGEventTap` keyboard grab and an
//! `NSEvent` momentum-phase monitor. Everywhere else the same API is provided
//! by no-op implementations, so the rest of the app has no `cfg` branches
//! around grabbing or momentum.
//!
//! The release-chord parser and the macOS keycode tables are portable data and
//! live here too: the parser has no OS calls and the Linux evdev grab will need
//! it later.

pub mod chord;
pub mod grab;
pub mod keymap;
pub mod momentum;
