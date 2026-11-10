//! No keyboard grab on this platform.
//!
//! The same API as the macOS implementation, doing nothing. The app can call it
//! unconditionally; [`supported`](super::supported) is how the startup code
//! decides whether to promise a grab at all.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use super::{Chord, GrabEvent};

/// No-op handle, kept so `App` can hold `Option<grab::Grab>` on every platform.
pub struct Grab;

impl Grab {
    pub fn set_enabled(&self, _on: bool) {}
    #[allow(dead_code)]
    pub fn is_enabled(&self) -> bool {
        false
    }
}

pub fn request_permission() {}

pub fn start<F>(_on_event: F, _chord: Chord) -> Result<Grab, String>
where
    F: Fn(GrabEvent) + Send + Sync + 'static,
{
    Ok(Grab)
}

pub fn is_active() -> bool {
    false
}

pub fn beat_ui() {}

pub fn beat_reader() {}

pub fn set_reader_blocked(_flag: Arc<AtomicBool>) {}

pub fn note(_what: &str) {}
