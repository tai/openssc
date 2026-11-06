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

// CGEventFlags (device-independent).
pub const FLAG_SHIFT: u64 = 0x0002_0000;
pub const FLAG_CONTROL: u64 = 0x0004_0000;
pub const FLAG_OPTION: u64 = 0x0008_0000;
pub const FLAG_COMMAND: u64 = 0x0010_0000;
const FLAG_FN: u64 = 0x0080_0000;
const FLAG_CAPS: u64 = 0x0001_0000;
// Device-dependent left/right bits (IOLLEvent.h).
const DEV_LEFT_SHIFT: u64 = 0x0000_0002;
const DEV_LEFT_CMD: u64 = 0x0000_0008;

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

/// The release chord.
#[derive(Debug, Clone, Copy)]
pub enum Chord {
    /// Modifier-only hold, e.g. Left Command + Shift for 350 ms.
    Hold {
        command: bool,
        shift: bool,
        control: bool,
        option: bool,
        left_command: bool,
        hold: Duration,
    },
    /// A key pressed with modifiers, e.g. Ctrl+Option+Cmd+Esc.
    Key {
        keycode: u16,
        command: bool,
        shift: bool,
        control: bool,
        option: bool,
    },
}

impl Chord {
    pub fn default_hold() -> Chord {
        Chord::Hold {
            command: true,
            shift: true,
            control: false,
            option: false,
            left_command: true,
            hold: Duration::from_millis(350),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Chord::Hold {
                command,
                shift,
                control,
                option,
                left_command,
                hold,
            } => {
                let mut parts = Vec::new();
                if *control {
                    parts.push("Ctrl");
                }
                if *option {
                    parts.push("Option");
                }
                if *command {
                    parts.push(if *left_command { "Left-Cmd" } else { "Cmd" });
                }
                if *shift {
                    parts.push("Shift");
                }
                format!("hold {} {} ms", parts.join("+"), hold.as_millis())
            }
            Chord::Key {
                keycode,
                command,
                shift,
                control,
                option,
            } => {
                let mut parts = Vec::new();
                if *control {
                    parts.push("Ctrl".to_string());
                }
                if *option {
                    parts.push("Option".to_string());
                }
                if *command {
                    parts.push("Cmd".to_string());
                }
                if *shift {
                    parts.push("Shift".to_string());
                }
                parts.push(keycode_name(*keycode));
                parts.join("+")
            }
        }
    }
}

fn keycode_name(keycode: u16) -> String {
    match keycode {
        0x35 => "Esc".to_string(),
        0x7a => "F1".to_string(),
        0x78 => "F2".to_string(),
        0x63 => "F3".to_string(),
        0x76 => "F4".to_string(),
        0x60 => "F5".to_string(),
        0x61 => "F6".to_string(),
        0x62 => "F7".to_string(),
        0x64 => "F8".to_string(),
        0x65 => "F9".to_string(),
        0x6d => "F10".to_string(),
        0x67 => "F11".to_string(),
        0x6f => "F12".to_string(),
        other => format!("keycode 0x{other:02x}"),
    }
}

/// Parse `--release-grab`, e.g. `cmd+shift`, `ctrl+alt+cmd+esc`, `f12`.
pub fn parse_chord(spec: &str) -> Result<Chord, String> {
    let mut command = false;
    let mut shift = false;
    let mut control = false;
    let mut option = false;
    let mut key: Option<u16> = None;
    for raw in spec.split('+') {
        let part = raw.trim().to_ascii_lowercase();
        match part.as_str() {
            "" => continue,
            "cmd" | "command" | "super" | "meta" => command = true,
            "shift" => shift = true,
            "ctrl" | "control" => control = true,
            "alt" | "opt" | "option" => option = true,
            "esc" | "escape" => key = Some(0x35),
            other => {
                if let Some(f) = other.strip_prefix('f') {
                    if let Ok(n) = f.parse::<u16>() {
                        let code = match n {
                            1 => 0x7a,
                            2 => 0x78,
                            3 => 0x63,
                            4 => 0x76,
                            5 => 0x60,
                            6 => 0x61,
                            7 => 0x62,
                            8 => 0x64,
                            9 => 0x65,
                            10 => 0x6d,
                            11 => 0x67,
                            12 => 0x6f,
                            _ => return Err(format!("unknown function key F{n}")),
                        };
                        key = Some(code);
                        continue;
                    }
                }
                if other.len() == 1 {
                    let c = other.chars().next().unwrap();
                    key = Some(char_keycode(c).ok_or_else(|| {
                        format!("no keycode for {c:?} in the release chord")
                    })?);
                    continue;
                }
                return Err(format!("unknown release-chord part {other:?}"));
            }
        }
    }
    match key {
        Some(keycode) => Ok(Chord::Key {
            keycode,
            command,
            shift,
            control,
            option,
        }),
        None if command || shift || control || option => Ok(Chord::Hold {
            command,
            shift,
            control,
            option,
            left_command: command,
            hold: Duration::from_millis(350),
        }),
        None => Err("release chord needs at least one modifier or key".to_string()),
    }
}

fn char_keycode(c: char) -> Option<u16> {
    Some(match c {
        'a' => 0x00,
        's' => 0x01,
        'd' => 0x02,
        'f' => 0x03,
        'h' => 0x04,
        'g' => 0x05,
        'z' => 0x06,
        'x' => 0x07,
        'c' => 0x08,
        'v' => 0x09,
        'b' => 0x0b,
        'q' => 0x0c,
        'w' => 0x0d,
        'e' => 0x0e,
        'r' => 0x0f,
        'y' => 0x10,
        't' => 0x11,
        '1' => 0x12,
        '2' => 0x13,
        '3' => 0x14,
        '4' => 0x15,
        '6' => 0x16,
        '5' => 0x17,
        '=' => 0x18,
        '9' => 0x19,
        '7' => 0x1a,
        '-' => 0x1b,
        '8' => 0x1c,
        '0' => 0x1d,
        ']' => 0x1e,
        'o' => 0x1f,
        'u' => 0x20,
        '[' => 0x21,
        'i' => 0x22,
        'p' => 0x23,
        'l' => 0x25,
        'j' => 0x26,
        '\'' => 0x27,
        'k' => 0x28,
        ';' => 0x29,
        '\\' => 0x2a,
        ',' => 0x2b,
        '/' => 0x2c,
        'n' => 0x2d,
        'm' => 0x2e,
        '.' => 0x2f,
        '`' => 0x32,
        _ => return None,
    })
}

/// Translate the tap's modifier flags into winit's state.
pub fn modifiers_from_flags(flags: u64) -> winit::keyboard::ModifiersState {
    use winit::keyboard::ModifiersState;
    let mut state = ModifiersState::empty();
    if flags & FLAG_SHIFT != 0 {
        state |= ModifiersState::SHIFT;
    }
    if flags & FLAG_CONTROL != 0 {
        state |= ModifiersState::CONTROL;
    }
    if flags & FLAG_OPTION != 0 {
        state |= ModifiersState::ALT;
    }
    if flags & FLAG_COMMAND != 0 {
        state |= ModifiersState::SUPER;
    }
    state
}

/// macOS virtual keycode -> winit `KeyCode`, i.e. the physical position the
/// layout tables are keyed by. Same table winit uses for its own events.
pub fn keycode_to_winit(keycode: u16) -> Option<winit::keyboard::KeyCode> {
    use winit::keyboard::KeyCode;
    Some(match keycode {
        0x00 => KeyCode::KeyA,
        0x01 => KeyCode::KeyS,
        0x02 => KeyCode::KeyD,
        0x03 => KeyCode::KeyF,
        0x04 => KeyCode::KeyH,
        0x05 => KeyCode::KeyG,
        0x06 => KeyCode::KeyZ,
        0x07 => KeyCode::KeyX,
        0x08 => KeyCode::KeyC,
        0x09 => KeyCode::KeyV,
        0x0b => KeyCode::KeyB,
        0x0c => KeyCode::KeyQ,
        0x0d => KeyCode::KeyW,
        0x0e => KeyCode::KeyE,
        0x0f => KeyCode::KeyR,
        0x10 => KeyCode::KeyY,
        0x11 => KeyCode::KeyT,
        0x12 => KeyCode::Digit1,
        0x13 => KeyCode::Digit2,
        0x14 => KeyCode::Digit3,
        0x15 => KeyCode::Digit4,
        0x16 => KeyCode::Digit6,
        0x17 => KeyCode::Digit5,
        0x18 => KeyCode::Equal,
        0x19 => KeyCode::Digit9,
        0x1a => KeyCode::Digit7,
        0x1b => KeyCode::Minus,
        0x1c => KeyCode::Digit8,
        0x1d => KeyCode::Digit0,
        0x1e => KeyCode::BracketRight,
        0x1f => KeyCode::KeyO,
        0x20 => KeyCode::KeyU,
        0x21 => KeyCode::BracketLeft,
        0x22 => KeyCode::KeyI,
        0x23 => KeyCode::KeyP,
        0x24 => KeyCode::Enter,
        0x25 => KeyCode::KeyL,
        0x26 => KeyCode::KeyJ,
        0x27 => KeyCode::Quote,
        0x28 => KeyCode::KeyK,
        0x29 => KeyCode::Semicolon,
        0x2a => KeyCode::Backslash,
        0x2b => KeyCode::Comma,
        0x2c => KeyCode::Slash,
        0x2d => KeyCode::KeyN,
        0x2e => KeyCode::KeyM,
        0x2f => KeyCode::Period,
        0x30 => KeyCode::Tab,
        0x31 => KeyCode::Space,
        0x32 => KeyCode::Backquote,
        0x33 => KeyCode::Backspace,
        0x35 => KeyCode::Escape,
        0x36 => KeyCode::SuperRight,
        0x37 => KeyCode::SuperLeft,
        0x38 => KeyCode::ShiftLeft,
        0x39 => KeyCode::CapsLock,
        0x3a => KeyCode::AltLeft,
        0x3b => KeyCode::ControlLeft,
        0x3c => KeyCode::ShiftRight,
        0x3d => KeyCode::AltRight,
        0x3e => KeyCode::ControlRight,
        0x3f => KeyCode::Fn,
        0x40 => KeyCode::F17,
        0x41 => KeyCode::NumpadDecimal,
        0x43 => KeyCode::NumpadMultiply,
        0x45 => KeyCode::NumpadAdd,
        0x47 => KeyCode::NumLock,
        0x49 => KeyCode::AudioVolumeUp,
        0x4a => KeyCode::AudioVolumeDown,
        0x4b => KeyCode::NumpadDivide,
        0x4c => KeyCode::NumpadEnter,
        0x4e => KeyCode::NumpadSubtract,
        0x4f => KeyCode::F18,
        0x50 => KeyCode::F19,
        0x51 => KeyCode::NumpadEqual,
        0x52 => KeyCode::Numpad0,
        0x53 => KeyCode::Numpad1,
        0x54 => KeyCode::Numpad2,
        0x55 => KeyCode::Numpad3,
        0x56 => KeyCode::Numpad4,
        0x57 => KeyCode::Numpad5,
        0x58 => KeyCode::Numpad6,
        0x59 => KeyCode::Numpad7,
        0x5a => KeyCode::F20,
        0x5b => KeyCode::Numpad8,
        0x5c => KeyCode::Numpad9,
        0x5d => KeyCode::IntlYen,
        0x5e => KeyCode::IntlRo,
        0x60 => KeyCode::F5,
        0x61 => KeyCode::F6,
        0x62 => KeyCode::F7,
        0x63 => KeyCode::F3,
        0x64 => KeyCode::F8,
        0x65 => KeyCode::F9,
        0x66 => KeyCode::Lang2,
        0x67 => KeyCode::F11,
        0x68 => KeyCode::Lang1,
        0x69 => KeyCode::F13,
        0x6a => KeyCode::F16,
        0x6b => KeyCode::F14,
        0x6d => KeyCode::F10,
        0x6f => KeyCode::F12,
        0x71 => KeyCode::F15,
        0x72 => KeyCode::Insert,
        0x73 => KeyCode::Home,
        0x74 => KeyCode::PageUp,
        0x75 => KeyCode::Delete,
        0x76 => KeyCode::F4,
        0x77 => KeyCode::End,
        0x78 => KeyCode::F2,
        0x79 => KeyCode::PageDown,
        0x7a => KeyCode::F1,
        0x7b => KeyCode::ArrowLeft,
        0x7c => KeyCode::ArrowRight,
        0x7d => KeyCode::ArrowDown,
        0x7e => KeyCode::ArrowUp,
        _ => return None,
    })
}

/// The logical key for a tap event: non-character keys by keycode, everything
/// else from the composed string CGEvent gives us.
pub fn logical_for(keycode: u16, chars: Option<&str>) -> winit::keyboard::Key {
    use winit::keyboard::{Key, NamedKey};
    let named = match keycode {
        0x24 => Some(NamedKey::Enter),
        0x30 => Some(NamedKey::Tab),
        0x33 => Some(NamedKey::Backspace),
        0x35 => Some(NamedKey::Escape),
        0x4c => Some(NamedKey::Enter),
        0x75 => Some(NamedKey::Delete),
        0x7b => Some(NamedKey::ArrowLeft),
        0x7c => Some(NamedKey::ArrowRight),
        0x7d => Some(NamedKey::ArrowDown),
        0x7e => Some(NamedKey::ArrowUp),
        0x73 => Some(NamedKey::Home),
        0x77 => Some(NamedKey::End),
        0x74 => Some(NamedKey::PageUp),
        0x79 => Some(NamedKey::PageDown),
        0x7a => Some(NamedKey::F1),
        0x78 => Some(NamedKey::F2),
        0x63 => Some(NamedKey::F3),
        0x76 => Some(NamedKey::F4),
        0x60 => Some(NamedKey::F5),
        0x61 => Some(NamedKey::F6),
        0x62 => Some(NamedKey::F7),
        0x64 => Some(NamedKey::F8),
        0x65 => Some(NamedKey::F9),
        0x6d => Some(NamedKey::F10),
        0x67 => Some(NamedKey::F11),
        0x6f => Some(NamedKey::F12),
        _ => None,
    };
    if let Some(named) = named {
        return Key::Named(named);
    }
    match chars {
        Some(s) if !s.is_empty() => Key::Character(s.to_string().into()),
        _ => Key::Unidentified(winit::keyboard::NativeKeyCode::MacOS(keycode).into()),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_default_hold_and_key_chords() {
        let hold = parse_chord("cmd+shift").unwrap();
        assert!(matches!(
            hold,
            Chord::Hold {
                command: true,
                shift: true,
                control: false,
                option: false,
                ..
            }
        ));
        assert!(hold.describe().contains("Left-Cmd+Shift"));
        let key = parse_chord("ctrl+alt+cmd+esc").unwrap();
        match key {
            Chord::Key {
                keycode,
                command,
                control,
                option,
                shift,
            } => {
                assert_eq!(keycode, 0x35);
                assert!(command && control && option && !shift);
            }
            _ => panic!("expected a key chord"),
        }
        assert!(parse_chord("bogus").is_err());
    }
}
