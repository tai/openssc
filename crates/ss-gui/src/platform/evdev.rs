//! Trackpad-grab seam.
//!
//! Linux exposes the evdev device handle. Other platforms have no raw evdev
//! path, so the same small API is a no-op there and `main.rs` needs no cfg
//! branches around focus, exit or the UI heartbeat.

#[cfg(target_os = "linux")]
pub use gesture_input::linux::DeviceHandle as GrabHandle;

#[cfg(not(target_os = "linux"))]
#[derive(Clone, Default)]
pub struct GrabHandle;

#[cfg(not(target_os = "linux"))]
impl GrabHandle {
    #[allow(dead_code)]
    pub fn set_grabbed(&self, _on: bool) -> std::io::Result<()> {
        Ok(())
    }

    #[allow(dead_code)]
    pub fn is_grabbed(&self) -> bool {
        false
    }

    #[allow(dead_code)]
    pub fn beat(&self) {}

    #[allow(dead_code)]
    pub fn name(&self) -> &str {
        "none"
    }
}
