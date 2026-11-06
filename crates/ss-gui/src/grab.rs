//! macOS keyboard grab: a `CGEventTap` that consumes keyboard events while our
//! window is key, so local global shortcuts (Ctrl+Space input switching,
//! Cmd+Space, Cmd+Tab, …) reach the remote instead of being eaten by macOS.
//!
//! Keyboard only. The pointer is deliberately not grabbed: consuming mouse
//! events at the session tap would break winit's own pointer handling,
//! click-to-focus and the local cursor, and there would be no way to click
//! another window to leave. Mouse events are not in the mask at all.
//!
//! The tap is enabled only while the window has focus. A hold chord releases
//! it (default: Left Command + Shift held for 350 ms) so nobody gets locked
//! out; the chord itself is passed through, never consumed, never forwarded.


use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use core_foundation::base::TCFType;
use core_foundation::mach_port::CFMachPort;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};

pub use crate::platform::chord::{parse_chord, Chord};
use crate::platform::keymap::{
    DEV_LEFT_CMD, DEV_LEFT_SHIFT, FLAG_CAPS, FLAG_COMMAND, FLAG_CONTROL, FLAG_FN, FLAG_OPTION,
    FLAG_SHIFT,
};
pub use crate::platform::keymap::{keycode_to_winit, logical_for, modifiers_from_flags};

// CGEventTapCreate arguments.
const K_CG_SESSION_EVENT_TAP: u32 = 1;
const K_CG_HEAD_INSERT_EVENT_TAP: u32 = 0;
const K_CG_EVENT_TAP_OPTION_DEFAULT: u32 = 0;

// CGEventType.
const KEY_DOWN: u32 = 10;
const KEY_UP: u32 = 11;
const FLAGS_CHANGED: u32 = 12;
const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

// CGEventField.
const KEYBOARD_EVENT_KEYCODE: u32 = 9;
const KEYBOARD_EVENT_AUTOREPEAT: u32 = 8;
type CGEventRef = *mut c_void;
type CGEventTapProxy = *mut c_void;
type CFMachPortRef = *mut c_void;

type TapCallback =
    extern "C" fn(CGEventTapProxy, u32, CGEventRef, *mut c_void) -> CGEventRef;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: TapCallback,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventGetFlags(event: CGEventRef) -> u64;
    fn CGEventKeyboardGetUnicodeString(
        event: CGEventRef,
        max_len: usize,
        actual_len: *mut usize,
        buf: *mut u16,
    ) -> bool;
}

/// What the tap sends back to the app.
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
/// The tap handle the app keeps.
pub struct Grab {
    shared: Arc<Shared>,
}

/// Last time the application proved it was alive, in milliseconds since the
/// process started. The tap watchdog uses it so a wedged app cannot keep the
/// keyboard captured.
static UI_HEARTBEAT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static READER_HEARTBEAT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Set by the session reader while its socket read is blocking. Waiting for a
/// server that has nothing to send is normal (push mode on a still screen) and
/// must not be reported as a stalled reader.
static READER_BLOCKED_FLAG: std::sync::OnceLock<std::sync::Arc<std::sync::atomic::AtomicBool>> =
    std::sync::OnceLock::new();

/// Whether the keyboard is currently captured. Mirrors `Shared::enabled` so
/// the event loop can ask without holding the handle.
static GRAB_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn is_active() -> bool {
    GRAB_ACTIVE.load(Ordering::Relaxed)
}

/// Called from the event loop. Cheap enough to call on every wake-up.
pub fn beat_ui() {
    UI_HEARTBEAT_MS.store(now_ms(), Ordering::Relaxed);
}

/// Called from the session reader loop.
pub fn beat_reader() {
    READER_HEARTBEAT_MS.store(now_ms(), Ordering::Relaxed);
}

/// Share the reader's "blocked in recv" flag with the watchdog. The reader
/// flips it around its blocking socket read.
pub fn set_reader_blocked(flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    let _ = READER_BLOCKED_FLAG.set(flag);
}

fn reader_blocked() -> bool {
    READER_BLOCKED_FLAG
        .get()
        .map(|f| f.load(Ordering::Relaxed))
        .unwrap_or(false)
}

/// Most recent events, so a stall report can say what the app was doing.
static RECENT: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Record one line of recent activity for the stall report (bounded).
pub fn note(what: &str) {
    if let Ok(mut v) = RECENT.lock() {
        if v.len() >= 24 {
            v.remove(0);
        }
        v.push(what.to_string());
    }
}

fn recent_lines() -> Vec<String> {
    RECENT.lock().map(|v| v.clone()).unwrap_or_default()
}

fn now_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// If the application stops ticking for this long while the keyboard is
/// captured, the grab is released. A keyboard grab that outlives its
/// application is a lockout: the user cannot even type the release chord.
const WATCHDOG_STALL: std::time::Duration = std::time::Duration::from_millis(3000);

impl Grab {
    pub fn set_enabled(&self, on: bool) {
        self.shared.enabled.store(on, Ordering::SeqCst);
        GRAB_ACTIVE.store(on, Ordering::SeqCst);
        let tap = self.shared.tap.load(Ordering::SeqCst);
        if tap != 0 {
            unsafe { CGEventTapEnable(tap as CFMachPortRef, on) };
        }
    }

    #[allow(dead_code)]
    pub fn is_enabled(&self) -> bool {
        self.shared.enabled.load(Ordering::SeqCst)
    }
}

struct Shared {
    on_event: Box<dyn Fn(GrabEvent) + Send + Sync>,
    chord: Chord,
    tap: AtomicUsize,
    enabled: AtomicBool,
    /// Bumped whenever the hold is cancelled, so a pending timer can bail.
    generation: AtomicU64,
    chord_held: AtomicBool,
    other_key_down: AtomicBool,
    hold_start: std::sync::Mutex<Option<Instant>>,
}

/// Ask for Accessibility permission once; the prompt is the only way the owner
/// learns the pane name. Never gates any feature.
pub fn request_permission() {
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::string::CFString;

    extern "C" {
        fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
        static kAXTrustedCheckOptionPrompt: core_foundation::string::CFStringRef;
    }
    let key = unsafe { CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt) };
    let dict = CFDictionary::from_CFType_pairs(&[(
        key.as_CFType(),
        CFBoolean::true_value().as_CFType(),
    )]);
    let trusted = unsafe { AXIsProcessTrustedWithOptions(dict.as_concrete_TypeRef()) };
    if !trusted {
        eprintln!("grab: Accessibility permission is not granted yet (prompt shown once)");
    }
}

/// Create the tap and start its run loop on a dedicated thread.
///
/// Returns `Err` with a human-readable reason when the tap cannot be created
/// (usually a missing Accessibility/Input Monitoring permission); the caller
/// logs it and continues without the grab.
pub fn start<F>(on_event: F, chord: Chord) -> Result<Grab, String>
where
    F: Fn(GrabEvent) + Send + Sync + 'static,
{
    let shared = Arc::new(Shared {
        on_event: Box::new(on_event),
        chord,
        tap: AtomicUsize::new(0),
        enabled: AtomicBool::new(false),
        generation: AtomicU64::new(0),
        chord_held: AtomicBool::new(false),
        other_key_down: AtomicBool::new(false),
        hold_start: std::sync::Mutex::new(None),
    });

    let user = Arc::as_ptr(&shared) as *mut c_void;
    // The tap outlives every handle; leak one strong reference for the callback.
    std::mem::forget(shared.clone());

    let mask = (1u64 << KEY_DOWN) | (1u64 << KEY_UP) | (1u64 << FLAGS_CHANGED);
    let tap = unsafe {
        CGEventTapCreate(
            K_CG_SESSION_EVENT_TAP,
            K_CG_HEAD_INSERT_EVENT_TAP,
            K_CG_EVENT_TAP_OPTION_DEFAULT,
            mask,
            tap_callback,
            user,
        )
    };
    if tap.is_null() {
        return Err("CGEventTapCreate returned NULL (permission)".to_string());
    }
    shared.tap.store(tap as usize, Ordering::SeqCst);

    // Watchdog: an unresponsive app must not keep the keyboard. Runs on its
    // own thread and only touches atomics plus the tap handle, never the app.
    {
        let watch = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("grab-watchdog".to_string())
            .spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_millis(500));
                if !watch.enabled.load(Ordering::SeqCst) {
                    continue;
                }
                let now = now_ms();
                let ui_stalled = now.saturating_sub(UI_HEARTBEAT_MS.load(Ordering::Relaxed));
                let rd_stalled = if reader_blocked() {
                    // Blocked waiting for the server: alive, not wedged.
                    0
                } else {
                    now.saturating_sub(READER_HEARTBEAT_MS.load(Ordering::Relaxed))
                };
                let limit = WATCHDOG_STALL.as_millis() as u64;
                if ui_stalled > limit || rd_stalled > limit {
                    let what = if ui_stalled > limit { "event loop" } else { "session reader" };
                    eprintln!(
                        "grab: {what} stalled for {} ms (ui {} ms, reader {} ms); releasing the keyboard so it cannot be a lockout",
                        ui_stalled.max(rd_stalled),
                        ui_stalled,
                        rd_stalled
                    );
                    for line in recent_lines() {
                        eprintln!("grab:   recent: {line}");
                    }
                    watch.enabled.store(false, Ordering::SeqCst);
                    let tap = watch.tap.load(Ordering::SeqCst);
                    if tap != 0 {
                        unsafe { CGEventTapEnable(tap as CFMachPortRef, false) };
                    }
                }
            })
            .ok();
    }

    let thread_shared = Arc::clone(&shared);
    let tap_addr = tap as usize;
    std::thread::Builder::new()
        .name("grab-tap".to_string())
        .spawn(move || unsafe {
            let port = CFMachPort::wrap_under_create_rule(tap_addr as *mut _);
            let source = match port.create_runloop_source(0) {
                Ok(source) => source,
                Err(()) => {
                    eprintln!("grab: could not create the tap run-loop source");
                    return;
                }
            };
            let rl = CFRunLoop::get_current();
            rl.add_source(&source, kCFRunLoopCommonModes);
            let _ = thread_shared;
            CFRunLoop::run_current();
        })
        .map_err(|e| format!("could not start the grab thread: {e}"))?;

    Ok(Grab { shared })
}

fn flags_state(flags: u64) -> (bool, bool, bool, bool) {
    (
        flags & FLAG_COMMAND != 0,
        flags & FLAG_SHIFT != 0,
        flags & FLAG_CONTROL != 0,
        flags & FLAG_OPTION != 0,
    )
}

fn is_chord_flags(flags: u64, command: bool, shift: bool, control: bool, option: bool) -> bool {
    let (c, s, k, o) = flags_state(flags);
    c == command && s == shift && k == control && o == option
}

/// Modifier key codes: their keyDown/keyUp must not cancel the hold chord
/// (some remappers and synthetic events send them alongside flagsChanged).
fn is_modifier_keycode(keycode: u16) -> bool {
    // 0x36..0x3f is exactly the modifier block in the Apple keycode space.
    (0x36..=0x3f).contains(&keycode)
}

fn chord_hold_matches(shared: &Shared, flags: u64) -> bool {
    match shared.chord {
        Chord::Hold {
            command,
            shift,
            control,
            option,
            left_command,
            ..
        } => {
            if !is_chord_flags(flags, command, shift, control, option) {
                return false;
            }
            // Reject other modifiers that would make it a different chord.
            if flags & (FLAG_FN | FLAG_CAPS) != 0 {
                return false;
            }
            if left_command && flags & DEV_LEFT_CMD == 0 {
                return false;
            }
            if shift && flags & DEV_LEFT_SHIFT == 0 && flags & FLAG_SHIFT == 0 {
                return false;
            }
            !shared.other_key_down.load(Ordering::SeqCst)
        }
        Chord::Key { .. } => false,
    }
}

fn start_hold_timer(shared: &Arc<Shared>, start: Instant) {
    let generation = shared.generation.fetch_add(1, Ordering::SeqCst) + 1;
    let shared = Arc::clone(shared);
    let hold = match shared.chord {
        Chord::Hold { hold, .. } => hold,
        Chord::Key { .. } => return,
    };
    std::thread::spawn(move || {
        std::thread::sleep(hold);
        if shared.generation.load(Ordering::SeqCst) != generation {
            return;
        }
        if !shared.chord_held.load(Ordering::SeqCst) || !shared.enabled.load(Ordering::SeqCst) {
            return;
        }
        // Still held: release the grab. The tap is disabled from here on, so
        // the pending key-ups pass through to the local system.
        shared.enabled.store(false, Ordering::SeqCst);
        let tap = shared.tap.load(Ordering::SeqCst);
        if tap != 0 {
            unsafe { CGEventTapEnable(tap as CFMachPortRef, false) };
        }
        (shared.on_event)(GrabEvent::Released {
            held: start.elapsed(),
        });
    });
}

extern "C" fn tap_callback(
    _proxy: CGEventTapProxy,
    etype: u32,
    event: CGEventRef,
    user: *mut c_void,
) -> CGEventRef {
    // The forgotten Arc keeps one strong reference alive for the tap's
    // lifetime; take it, then put it back for the next callback.
    let shared = unsafe { Arc::from_raw(user as *const Shared) };
    let _ = Arc::into_raw(Arc::clone(&shared));

    if etype == TAP_DISABLED_BY_TIMEOUT || etype == TAP_DISABLED_BY_USER_INPUT {
        let reason = if etype == TAP_DISABLED_BY_TIMEOUT {
            "timeout"
        } else {
            "user input"
        };
        // Only revive a tap that is supposed to be grabbing: re-enabling it
        // after the release chord or a focus loss would silently re-grab.
        if shared.enabled.load(Ordering::SeqCst) {
            let tap = shared.tap.load(Ordering::SeqCst);
            if tap != 0 {
                unsafe { CGEventTapEnable(tap as CFMachPortRef, true) };
            }
            (shared.on_event)(GrabEvent::Reenabled { reason });
        }
        return event;
    }

    if event.is_null() {
        return event;
    }

    // Nothing is consumed or tracked while the grab is off: the chord only
    // means something while we own the keyboard.
    if !shared.enabled.load(Ordering::SeqCst) {
        return event;
    }

    let flags = unsafe { CGEventGetFlags(event) };
    let keycode = unsafe { CGEventGetIntegerValueField(event, KEYBOARD_EVENT_KEYCODE) } as u16;
    let pressed = etype == KEY_DOWN;
    let repeat = unsafe {
        CGEventGetIntegerValueField(event, KEYBOARD_EVENT_AUTOREPEAT) != 0
    };

    let mut chord_consumed = false;
    match shared.chord {
        Chord::Hold {
            command,
            shift,
            control,
            option,
            left_command,
            hold,
        } => {
            if etype == FLAGS_CHANGED {
                let matches = chord_hold_matches(&shared, flags);
                if matches && !shared.chord_held.load(Ordering::SeqCst) {
                    shared.chord_held.store(true, Ordering::SeqCst);
                    shared.other_key_down.store(false, Ordering::SeqCst);
                    let now = Instant::now();
                    if let Ok(mut start) = shared.hold_start.lock() {
                        *start = Some(now);
                    }
                    (shared.on_event)(GrabEvent::Key {
                        keycode,
                        chars: None,
                        flags,
                        pressed: true,
                        repeat: false,
                        modifier_only: true,
                    });
                    eprintln!(
                        "grab: release chord held ({}), waiting {} ms",
                        shared.chord.describe(),
                        hold.as_millis()
                    );
                    start_hold_timer(&shared, now);
                } else if !matches && shared.chord_held.load(Ordering::SeqCst) {
                    // One of the chord's modifiers was let go before the hold
                    // completed.
                    shared.chord_held.store(false, Ordering::SeqCst);
                    shared.generation.fetch_add(1, Ordering::SeqCst);
                    let held = shared
                        .hold_start
                        .lock()
                        .ok()
                        .and_then(|s| *s)
                        .map(|s| s.elapsed())
                        .unwrap_or_default();
                    (shared.on_event)(GrabEvent::HoldCancelled { held });
                }
                // The hold chord is local: its modifier events are consumed
                // and never forwarded, so the remote never sees Cmd+Shift for
                // a gesture it did not type. A cancelled hold (a real shortcut
                // like Cmd+Shift+V) is followed by the key event, whose flags
                // make the app re-sync and send the modifiers before the key.
                chord_consumed = shared.chord_held.load(Ordering::SeqCst);
                let _ = (command, shift, control, option, left_command);
            } else if etype == KEY_DOWN
                && !is_modifier_keycode(keycode)
                && shared.chord_held.load(Ordering::SeqCst)
            {
                // Any real key during the hold cancels it (Cmd+Shift+V must
                // still pull, not release the grab).
                shared.other_key_down.store(true, Ordering::SeqCst);
                shared.chord_held.store(false, Ordering::SeqCst);
                shared.generation.fetch_add(1, Ordering::SeqCst);
                let held = shared
                    .hold_start
                    .lock()
                    .ok()
                    .and_then(|s| *s)
                    .map(|s| s.elapsed())
                    .unwrap_or_default();
                (shared.on_event)(GrabEvent::HoldCancelled { held });
            }
        }
        Chord::Key {
            keycode: chord_code,
            command,
            shift,
            control,
            option,
        } => {
            if etype == KEY_DOWN
                && keycode == chord_code
                && is_chord_flags(flags, command, shift, control, option)
            {
                shared.enabled.store(false, Ordering::SeqCst);
                let tap = shared.tap.load(Ordering::SeqCst);
                if tap != 0 {
                    unsafe { CGEventTapEnable(tap as CFMachPortRef, false) };
                }
                (shared.on_event)(GrabEvent::Released {
                    held: Duration::ZERO,
                });
                // Pass the chord through to the local system.
                return event;
            }
        }
    }

    if chord_consumed {
        return std::ptr::null_mut();
    }

    // Consume and forward. The app applies the same mapping as winit's path.
    let chars = if etype == FLAGS_CHANGED {
        None
    } else {
        read_unicode(event)
    };
    (shared.on_event)(GrabEvent::Key {
        keycode,
        chars,
        flags,
        pressed,
        repeat,
        modifier_only: etype == FLAGS_CHANGED,
    });
    std::ptr::null_mut()
}

fn read_unicode(event: CGEventRef) -> Option<String> {
    let mut buf = [0u16; 8];
    let mut len = 0usize;
    let ok = unsafe {
        CGEventKeyboardGetUnicodeString(event, buf.len(), &mut len, buf.as_mut_ptr())
    };
    if !ok || len == 0 {
        return None;
    }
    let s = String::from_utf16_lossy(&buf[..len.min(buf.len())]);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}
