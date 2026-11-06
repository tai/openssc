//! Keyboard-grab seam.
//!
//! macOS implements this with a session-level `CGEventTap` (see `macos.rs`).
//! Every other platform gets a disabled no-op with the same API, so `main.rs`
//! calls `is_active`, `beat_ui`, `note` and friends unconditionally and only
//! asks [`supported`] when it wants to explain why the grab is off.
//!
//! The release-chord parser, [`Chord`] and the keycode tables are portable and
//! re-exported here so callers still say `grab::parse_chord`.

use std::time::Duration;

pub use crate::platform::chord::{parse_chord, Chord};
pub use crate::platform::keymap::{keycode_to_winit, logical_for, modifiers_from_flags};

/// What the tap sends back to the app.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, Clone)]
pub enum GrabEvent {
    /// A keyboard event that was consumed locally and must go to the remote.
    Key {
        keycode: u16,
        chars: Option<String>,
        flags: u64,
        pressed: bool,
        repeat: bool,
        /// flagsChanged: no character, only modifier state changed.
        modifier_only: bool,
    },
    /// The hold/key chord fired: the grab is off until the window loses and
    /// regains focus.
    Released { held: Duration },
    /// The chord was seen but cancelled (another key or modifier went down).
    HoldCancelled { held: Duration },
    /// macOS disabled the tap (timeout / user input); we re-enabled it.
    Reenabled { reason: &'static str },
}

/// Whether this platform has a keyboard grab at all.
pub fn supported() -> bool {
    cfg!(target_os = "macos")
}

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(not(target_os = "macos"))]
mod noop;
#[cfg(not(target_os = "macos"))]
pub use noop::*;
