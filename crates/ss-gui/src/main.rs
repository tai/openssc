//! Live Apple Screen Sharing client window.
//!
//! Runs on macOS for lab testing and on Linux/Wayland via winit + softbuffer.
//! The protocol crate is platform-neutral; this file only translates window
//! events into RFB/Apple messages and presents BGRX frames.

use anyhow::Result;
use gesture_input::{AxisFilter, Gesture, Phase, SCROLL_POINTS_PER_LINE};
use rfb_proto::mvs::{MvsDecoder, Rect};
use rfb_proto::{
    client_cut_text, clipboard_fetch, framebuffer_update_request, gesture_begin_end,
    gesture_magnify, gesture_mouse_scroll, gesture_rotate, gesture_scroll, gesture_smart_magnify,
    gesture_swipe,
    key_event, image_item, pasteboard_archive, pasteboard_image,
    pointer_event, viewer_clipboard_send, AppleSession, CursorUpdate, Update,
};
use std::fs::File;
use std::io::BufWriter;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{Window, WindowId};

use raw_window_handle::{DisplayHandle, HasDisplayHandle};

mod filecopy;

#[cfg(target_os = "macos")]
mod grab;
#[cfg(target_os = "macos")]
mod momentum;

/// The keyboard grab handle. A no-op stub off macOS so the rest of the app
/// does not need cfg branches.
#[cfg(target_os = "macos")]
type GrabHandle = grab::Grab;
#[cfg(not(target_os = "macos"))]
struct GrabHandle;
#[cfg(not(target_os = "macos"))]
impl GrabHandle {
    fn set_enabled(&self, _on: bool) {}
    fn is_enabled(&self) -> bool {
        false
    }
}

/// Scroll deltas accumulated between subtype-11 messages, in the portable
/// model's units (pixels).
struct PendingScroll {
    dx: f32,
    dy: f32,
    phase: Phase,
    momentum: Option<Phase>,
    precise: bool,
    /// Last sample's delta, for the debug log.
    raw: (f32, f32),
}

/// EXPERIMENT: which Apple scroll subtype to send for precise input.
/// 11 = line-unit MouseScroll (the verified path), 8 = gesture scroll with
/// pixel deltas (untested on this server). Set with `SS_SCROLL_SUBTYPE=8`.
static SCROLL_SUBTYPE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(11);

/// Scroll coalescing interval: ~120 Hz, so a fast flick does not flood the
/// session with one 58-byte message per trackpad sample.
const SCROLL_INTERVAL: Duration = Duration::from_millis(4);
/// Points of finger travel per scroll line. Measured on the lab server:
/// one subtype-11 line unit scrolls a TextEdit document ~120 points (three
/// consecutive isolated events, 240 screen px at 2x). The server applies its
/// own acceleration to multi-line deltas, so this is a 1:1 tracking choice.
const PIXELS_PER_LINE: f32 = SCROLL_POINTS_PER_LINE;

/// `--scroll-speed`: multiplier on scroll distance, for feel calibration.
/// Stored as f32 bits so it can be set from the CLI without threading it
/// through every constructor.
static SCROLL_SPEED: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0x3f80_0000); // 1.0f32

fn scroll_speed() -> f32 {
    f32::from_bits(SCROLL_SPEED.load(std::sync::atomic::Ordering::Relaxed))
}

/// Should a scroll event be forwarded to the remote?
///
/// Touchdown events always go through (that is the continuous scrolling the
/// owner wants smooth). OS momentum events are dropped by default - they are
/// what made micro-scroll and flick-and-stop drift - and forwarded only with
/// `--coast`. An event with no classification (monitor unavailable or a
/// desync) is forwarded rather than swallowed.
fn scroll_forward(is_momentum: Option<bool>, coast: bool) -> bool {
    match is_momentum {
        Some(true) => coast,
        Some(false) | None => true,
    }
}

/// Map winit's touch phase onto the portable gesture phase.
fn phase_from_touch(phase: TouchPhase) -> Phase {
    match phase {
        TouchPhase::Started => Phase::Began,
        TouchPhase::Moved => Phase::Changed,
        TouchPhase::Ended => Phase::Ended,
        TouchPhase::Cancelled => Phase::Cancelled,
    }
}

/// Synthesise one subtype-11 scroll message from the portable gesture model.
///
/// This is the only place scroll numbers become wire fields, so the macOS
/// winit backend and the Linux evdev recogniser produce identical messages.
/// `remainder` carries the sub-line fraction between messages so sub-line
/// motion is not lost. Every call returns a message: the point deltas carry
/// the smooth motion even when the integer line field is still zero.
fn scroll_message(
    gesture: &Gesture,
    x: u16,
    y: u16,
    remainder: (f32, f32),
) -> (Option<Vec<u8>>, (f32, f32), (i16, i16), (i32, i32)) {
    let Gesture::Scroll {
        dx,
        dy,
        phase,
        momentum,
        ..
    } = *gesture
    else {
        return (None, remainder, (0, 0), (0, 0));
    };
    let points_per_line = (PIXELS_PER_LINE / scroll_speed()).max(0.5);
    if SCROLL_SUBTYPE.load(std::sync::atomic::Ordering::Relaxed) == 8 {
        // Gesture scroll: the same finger deltas as f32 pixels, with the
        // gesture phase. No line conversion at all.
        let msg = gesture_scroll(dx, dy, 0.0, 1, phase.code(), 0, x, y);
        return (Some(msg), (0.0, 0.0), (0, 0), (0, 0));
    }
    // Native-client shape (measured from an SS.app capture: 295 records of
    // exactly the same 82-byte subtype-11 message, ~60 Hz, one per trackpad
    // event, including sub-line deltas):
    //
    // * precise (trackpad): line field = 0, the WHOLE delta in `fixedDelta`
    //   as 16.16 lines. The server accumulates the fractions; a locally
    //   truncated whole-line delta is what made the motion step-wise.
    // * wheel: the line fields carry the notches, fixed stays zero.
    //
    // Nothing is accumulated locally for precise input (the server does it),
    // so the remainder is always zero and cannot double-count.
    let precise = matches!(gesture, Gesture::Scroll { precise: true, .. });
    let (lx, ly, fx, fy) = if precise {
        let fx = ((dx / points_per_line) * 65536.0)
            .round()
            .clamp(-2_147_483_648.0, 2_147_483_647.0) as i32;
        let fy = ((dy / points_per_line) * 65536.0)
            .round()
            .clamp(-2_147_483_648.0, 2_147_483_647.0) as i32;
        (0i16, 0i16, fx, fy)
    } else {
        let lx = (dx / points_per_line).trunc().clamp(-40.0, 40.0) as i16;
        let ly = (dy / points_per_line).trunc().clamp(-40.0, 40.0) as i16;
        (lx, ly, 0i32, 0i32)
    };
    let msg = gesture_mouse_scroll(
        lx,
        ly,
        0,
        (fx, fy, 0),
        (0, 0, 0),
        phase.code(),
        // TODO(momentum): winit folds NSEvent's momentum phase into
        // TouchPhase, so macOS cannot fill this yet; the field exists for a
        // backend that can (an NSEvent monitor or the Linux recogniser).
        momentum.map(|m| m.code()).unwrap_or(0),
        x,
        y,
    );
    (Some(msg), (0.0, 0.0), (lx, ly), (fx, fy))
}

/// One Cmd+Shift+V pull: how many fetches went out and what the last archive
/// actually contained, so the refusal can quote it.
struct PullState {
    started: Instant,
    attempts: u8,
    last_seen: Option<String>,
}

/// Fetches per pull (the first plus two retries) and the spacing between them.
/// The whole pull decides within `PULL_WINDOW`, so a refusal still feels
/// instant.
const PULL_FETCHES: u8 = 3;
const PULL_RETRY: Duration = Duration::from_millis(180);
const PULL_WINDOW: Duration = Duration::from_millis(700);

/// One physical position in a layout: the character it produces plain and
/// with Shift. macOS key codes are positions, so this table is the layout.
type Row = (KeyCode, char, char);

struct Keymap {
    name: &'static str,
    rows: &'static [Row],
    /// Positions whose keysym is not a character (keypad, JIS input keys).
    special: &'static [(KeyCode, u32)],
}

/// macOS US layout, by key code.
static US_ROWS: &[Row] = &[
    (KeyCode::KeyA, 'a', 'A'),
    (KeyCode::KeyS, 's', 'S'),
    (KeyCode::KeyD, 'd', 'D'),
    (KeyCode::KeyF, 'f', 'F'),
    (KeyCode::KeyH, 'h', 'H'),
    (KeyCode::KeyG, 'g', 'G'),
    (KeyCode::KeyZ, 'z', 'Z'),
    (KeyCode::KeyX, 'x', 'X'),
    (KeyCode::KeyC, 'c', 'C'),
    (KeyCode::KeyV, 'v', 'V'),
    (KeyCode::KeyB, 'b', 'B'),
    (KeyCode::KeyQ, 'q', 'Q'),
    (KeyCode::KeyW, 'w', 'W'),
    (KeyCode::KeyE, 'e', 'E'),
    (KeyCode::KeyR, 'r', 'R'),
    (KeyCode::KeyY, 'y', 'Y'),
    (KeyCode::KeyT, 't', 'T'),
    (KeyCode::KeyO, 'o', 'O'),
    (KeyCode::KeyU, 'u', 'U'),
    (KeyCode::KeyI, 'i', 'I'),
    (KeyCode::KeyP, 'p', 'P'),
    (KeyCode::KeyL, 'l', 'L'),
    (KeyCode::KeyJ, 'j', 'J'),
    (KeyCode::KeyK, 'k', 'K'),
    (KeyCode::KeyN, 'n', 'N'),
    (KeyCode::KeyM, 'm', 'M'),
    (KeyCode::Digit1, '1', '!'),
    (KeyCode::Digit2, '2', '@'),
    (KeyCode::Digit3, '3', '#'),
    (KeyCode::Digit4, '4', '$'),
    (KeyCode::Digit5, '5', '%'),
    (KeyCode::Digit6, '6', '^'),
    (KeyCode::Digit7, '7', '&'),
    (KeyCode::Digit8, '8', '*'),
    (KeyCode::Digit9, '9', '('),
    (KeyCode::Digit0, '0', ')'),
    (KeyCode::Minus, '-', '_'),
    (KeyCode::Equal, '=', '+'),
    (KeyCode::BracketLeft, '[', '{'),
    (KeyCode::BracketRight, ']', '}'),
    (KeyCode::Backslash, '\\', '|'),
    (KeyCode::Semicolon, ';', ':'),
    (KeyCode::Quote, '\'', '"'),
    (KeyCode::Comma, ',', '<'),
    (KeyCode::Period, '.', '>'),
    (KeyCode::Slash, '/', '?'),
    (KeyCode::Backquote, '`', '~'),
    (KeyCode::IntlBackslash, '\\', '|'),
    (KeyCode::IntlRo, '\\', '_'),
];

/// Apple Japanese (JIS) layout, measured key by key against the lab server's
/// own keyboard: `@` at BracketLeft, `^` at Equal, `:` at Quote, `[` at
/// BracketRight, `]` at Backslash, `\` at IntlYen.
static JIS_ROWS: &[Row] = &[
    (KeyCode::KeyA, 'a', 'A'),
    (KeyCode::KeyS, 's', 'S'),
    (KeyCode::KeyD, 'd', 'D'),
    (KeyCode::KeyF, 'f', 'F'),
    (KeyCode::KeyH, 'h', 'H'),
    (KeyCode::KeyG, 'g', 'G'),
    (KeyCode::KeyZ, 'z', 'Z'),
    (KeyCode::KeyX, 'x', 'X'),
    (KeyCode::KeyC, 'c', 'C'),
    (KeyCode::KeyV, 'v', 'V'),
    (KeyCode::KeyB, 'b', 'B'),
    (KeyCode::KeyQ, 'q', 'Q'),
    (KeyCode::KeyW, 'w', 'W'),
    (KeyCode::KeyE, 'e', 'E'),
    (KeyCode::KeyR, 'r', 'R'),
    (KeyCode::KeyY, 'y', 'Y'),
    (KeyCode::KeyT, 't', 'T'),
    (KeyCode::KeyO, 'o', 'O'),
    (KeyCode::KeyU, 'u', 'U'),
    (KeyCode::KeyI, 'i', 'I'),
    (KeyCode::KeyP, 'p', 'P'),
    (KeyCode::KeyL, 'l', 'L'),
    (KeyCode::KeyJ, 'j', 'J'),
    (KeyCode::KeyK, 'k', 'K'),
    (KeyCode::KeyN, 'n', 'N'),
    (KeyCode::KeyM, 'm', 'M'),
    (KeyCode::Digit1, '1', '!'),
    (KeyCode::Digit2, '2', '"'),
    (KeyCode::Digit3, '3', '#'),
    (KeyCode::Digit4, '4', '$'),
    (KeyCode::Digit5, '5', '%'),
    (KeyCode::Digit6, '6', '&'),
    (KeyCode::Digit7, '7', '\''),
    (KeyCode::Digit8, '8', '('),
    (KeyCode::Digit9, '9', ')'),
    (KeyCode::Digit0, '0', '0'),
    (KeyCode::Minus, '-', '='),
    (KeyCode::Equal, '^', '~'),
    (KeyCode::BracketLeft, '@', '`'),
    (KeyCode::BracketRight, '[', '{'),
    (KeyCode::Backslash, ']', '}'),
    (KeyCode::Semicolon, ';', '+'),
    (KeyCode::Quote, ':', '*'),
    (KeyCode::Comma, ',', '<'),
    (KeyCode::Period, '.', '>'),
    (KeyCode::Slash, '/', '?'),
    (KeyCode::Backquote, '`', '~'),
    (KeyCode::IntlYen, '\\', '|'),
    (KeyCode::IntlRo, '_', '_'),
];

/// Keypad keys. Their keysyms are in the daemon's Pass A table
/// (0xFFB0..0xFFB9 -> 0x52..0x5C, 0xFFBD -> 0x51).
///
/// TODO(JIS): the daemon has no keysym for the JIS input keys (yen, henkan,
/// muhenkan, kana, eisu); pB's J.1/J.3 show Pass A and Pass B carry none of
/// them, so they are logged as unsupported instead of guessed at.
static SPECIAL_KEYS: &[(KeyCode, u32)] = &[
    (KeyCode::Numpad0, 0xffb0),
    (KeyCode::Numpad1, 0xffb1),
    (KeyCode::Numpad2, 0xffb2),
    (KeyCode::Numpad3, 0xffb3),
    (KeyCode::Numpad4, 0xffb4),
    (KeyCode::Numpad5, 0xffb5),
    (KeyCode::Numpad6, 0xffb6),
    (KeyCode::Numpad7, 0xffb7),
    (KeyCode::Numpad8, 0xffb8),
    (KeyCode::Numpad9, 0xffb9),
    (KeyCode::NumpadEnter, 0xff8d),
    (KeyCode::NumpadAdd, 0xffab),
    (KeyCode::NumpadSubtract, 0xffad),
    (KeyCode::NumpadMultiply, 0xffaa),
    (KeyCode::NumpadDivide, 0xffaf),
    (KeyCode::NumpadDecimal, 0xffae),
    (KeyCode::NumpadEqual, 0xffbd),
    (KeyCode::NumpadComma, 0xffac),
];

static US: Keymap = Keymap {
    name: "us",
    rows: US_ROWS,
    special: SPECIAL_KEYS,
};
static JIS: Keymap = Keymap {
    name: "jis",
    rows: JIS_ROWS,
    special: SPECIAL_KEYS,
};

impl Keymap {
    fn by_name(name: &str) -> Option<&'static Keymap> {
        match name {
            "us" => Some(&US),
            "jis" => Some(&JIS),
            _ => None,
        }
    }

    fn row(&self, code: KeyCode) -> Option<(char, char)> {
        self.rows
            .iter()
            .find(|(c, _, _)| *c == code)
            .map(|(_, plain, shifted)| (*plain, *shifted))
    }

    fn special(&self, code: KeyCode) -> Option<u32> {
        self.special
            .iter()
            .find(|(c, _)| *c == code)
            .map(|(_, ks)| *ks)
    }

    /// The inverse table: which position (and shift level) types `c`.
    fn char_position(&self, c: char) -> Option<(KeyCode, bool)> {
        for (code, plain, shifted) in self.rows {
            if *plain == c {
                return Some((*code, false));
            }
            if *shifted == c {
                return Some((*code, true));
            }
        }
        None
    }

    /// The keysym to put on the wire for a position at a shift level.
    ///
    /// Measured on the lab build (probe: 0x5B typed `[`, 0x40 typed `@`,
    /// Shift + 0x32 typed `2`): the daemon resolves a keysym through the
    /// REMOTE layout and ignores the Shift modifier for printable keys, so the
    /// wire form is this layout's character for the position at that shift
    /// level. A US-position keysym would type the wrong key (0x5B lands on the
    /// remote's `[` key, not its `@` key).
    fn wire_keysym(&self, code: KeyCode, shift: bool) -> Option<u32> {
        if let Some(ks) = self.special(code) {
            return Some(ks);
        }
        let (plain, shifted) = self.row(code)?;
        let c = if shift { shifted } else { plain };
        Some(c as u32)
    }
}

/// JIS input keys the daemon cannot express: pB's J.1/J.3 show no keysym for
/// them in Pass A or Pass B, so they are logged rather than guessed at.
fn is_unsupported_jis_code(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Lang1
            | KeyCode::Lang2
            | KeyCode::Lang3
            | KeyCode::Lang4
            | KeyCode::Lang5
            | KeyCode::Convert
            | KeyCode::NonConvert
            | KeyCode::KanaMode
    )
}

/// Resolve a typed character to a keysym on the TARGET (remote) layout.
///
/// The character comes from the event, already composed under the *local*
/// layout, so the local layout never needs to be named. `--keymap` therefore
/// means one thing: which layout the remote runs.
///
/// Order: the target's position for that character; then the physical
/// position (only when the target can type something there); then `None` so
/// the caller can log and fall back to a Unicode keysym.
fn character_keysym(
    map: &Keymap,
    code: Option<KeyCode>,
    c: char,
    shift: bool,
) -> Option<u32> {
    if let Some(code) = code {
        if let Some(special) = map.special(code) {
            return Some(special);
        }
    }
    if let Some((position, shifted)) = map.char_position(c) {
        return map.wire_keysym(position, shifted);
    }
    if let Some(code) = code {
        if map.row(code).is_some() {
            return map.wire_keysym(code, shift);
        }
    }
    None
}

/// A keysym for composed text (the `--ime` path only). Non-ASCII uses the X11
/// Unicode convention; this is the one place a bare character keysym is sent.
fn text_keysym(c: char) -> u32 {
    if c.is_ascii() {
        c as u32
    } else {
        0x0100_0000 | c as u32
    }
}

/// Usage text for `-h` / `--help`.
///
/// Keep this in sync with the parser below: `usage_documents_every_flag` scans
/// this file for flag literals and fails if one is missing here.
const USAGE: &str = "\
ss-gui — Apple Screen Sharing client (RFB 003.889 + Apple extensions)

Usage: ss-gui [options]

Connection:
  --host <host>            server host                         (default mm.local)
  --port <port>            server port                         (default 5900)
  --user <name>            account on the remote                (default tai)
  --password <password>    password for that account            (or SS_PASSWORD)

Display and codec:
  --scale <f>              present the framebuffer at this scale (default 1.0)
  --mvs                    advertise MultiVariant (0x3f3); server push by default
  --poll                   MVS request loop instead of server push
  --push                   accepted no-op (push is the default with --mvs)
  --encoding <id>          advertise a single framebuffer encoding (6 = Zlib)
  --bands <n>              banded refresh bands, 1-64 (default 8; forced to 1 in the MVS request loop)
  --pipeline <n>           framebuffer requests kept in flight, 1-8 (default 3)
  --auto                   experimental: server-pushed dirty rectangles

Input and keyboard:
  --keymap us|jis          layout the REMOTE runs (default us)
  --scroll-speed <f>       scroll distance multiplier, feel calibration (default 1.0)
  --coast                  forward the OS momentum tail (default: drop it)
  --no-coast               accepted no-op; dropping momentum is the default
  --ime                    enable IME; composed text arrives via Ime::Commit
  --evdev                  Linux only: raw multitouch gestures from /dev/input
  --no-grab                macOS: do not capture the keyboard; local shortcuts stay local
  --release-grab <chord>   macOS release chord, e.g. cmd+shift or ctrl+alt+cmd+esc
                           (default: hold Left-Cmd+Shift for 350 ms)

Clipboard and file copy:
  --clipboard              automatic duplex clipboard (default on)
  --no-clipboard           disable the automatic clipboard
  --send-clipboard         send the local clipboard on connect
  --pull <path>            pull an absolute remote path into ~/Downloads (repeatable)
  --send-file <path>       send a local file to the remote Downloads (repeatable)
  --recv-file <path>       request a remote file into ~/Downloads (repeatable)

Lab and debugging:
  --dump <path>            write the composited window to a PNG
  --dump-after <n>         dump after n frames (default 4)
  --exit-after-dump        exit once the dump is written
  --exit-after-secs <s>    exit after s seconds
  --debug-input            trace input, clipboard, keys and framebuffer timing
  --scroll-test <kind>     scripted gesture for the scroll model
  --demo-cursor            scripted pointer tour that dumps a PNG per cursor shape
  -h, --help               show this message and exit
";

fn named_keysym(key: &NamedKey) -> Option<u32> {
    Some(match key {
        NamedKey::Enter => 0xff0d,
        NamedKey::Backspace => 0xff08,
        NamedKey::Tab => 0xff09,
        NamedKey::Escape => 0xff1b,
        NamedKey::Space => 0x20,
        NamedKey::Delete => 0xffff,
        NamedKey::Insert => 0xff63,
        NamedKey::Home => 0xff50,
        NamedKey::End => 0xff57,
        NamedKey::PageUp => 0xff55,
        NamedKey::PageDown => 0xff56,
        NamedKey::ArrowLeft => 0xff51,
        NamedKey::ArrowUp => 0xff52,
        NamedKey::ArrowRight => 0xff53,
        NamedKey::ArrowDown => 0xff54,
        NamedKey::Shift => 0xffe1,
        NamedKey::Control => 0xffe3,
        NamedKey::Alt => 0xffe9,
        NamedKey::Super => 0xffeb,
        NamedKey::Meta => 0xffeb,
        NamedKey::CapsLock => 0xffe5,
        NamedKey::F1 => 0xffbe,
        NamedKey::F2 => 0xffbf,
        NamedKey::F3 => 0xffc0,
        NamedKey::F4 => 0xffc1,
        NamedKey::F5 => 0xffc2,
        NamedKey::F6 => 0xffc3,
        NamedKey::F7 => 0xffc4,
        NamedKey::F8 => 0xffc5,
        NamedKey::F9 => 0xffc6,
        NamedKey::F10 => 0xffc7,
        NamedKey::F11 => 0xffc8,
        NamedKey::F12 => 0xffc9,
        _ => return None,
    })
}

/// Read the local text clipboard. Uses the platform clipboard tools so the
/// crate stays dependency-free; returns `None` when unavailable.
/// Milliseconds since the first input, updated on every input event. The
/// reader thread polls the framebuffer faster while this is recent so input
/// feedback arrives without the round-trip latency of the idle cadence.
static LAST_INPUT_MS: AtomicU64 = AtomicU64::new(0);
static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// Framebuffer row of the last click, so the band containing it is requested
/// first. `u32::MAX` means "no pending focus".
static FOCUS_Y: AtomicU64 = AtomicU64::new(u64::MAX);
static READER_UPDATES: AtomicU64 = AtomicU64::new(0);
static READER_ALIVE_MS: AtomicU64 = AtomicU64::new(0);
/// Timestamp of the last update that actually painted rectangles.
static LAST_APPLIED_MS: AtomicU64 = AtomicU64::new(0);
/// Set once the first pushed frame has been painted.
static PUSH_PAINTED: AtomicBool = AtomicBool::new(false);
/// Timestamp of the oldest local input event that has not been answered by a
/// painted rectangle yet; `u64::MAX` means "nothing pending".
static INPUT_PENDING_MS: AtomicU64 = AtomicU64::new(u64::MAX);

/// Milliseconds since the process started (the `START` epoch).
fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn note_input() {
    let ms = now_ms();
    LAST_INPUT_MS.store(ms, Ordering::Relaxed);
    // Keep the oldest pending input: it bounds the feedback latency.
    let _ = INPUT_PENDING_MS.compare_exchange(u64::MAX, ms, Ordering::Relaxed, Ordering::Relaxed);
}

fn input_is_hot() -> bool {
    match START.get() {
        Some(start) => {
            let last = LAST_INPUT_MS.load(Ordering::Relaxed) as u128;
            let now = start.elapsed().as_millis();
            now.saturating_sub(last) < 250
        }
        None => false,
    }
}

fn local_clipboard_text() -> Option<String> {
    use std::io::Write;
    #[cfg(target_os = "macos")]
    let (cmd, args): (&str, &[&str]) = ("pbpaste", &[]);
    #[cfg(not(target_os = "macos"))]
    let (cmd, args): (&str, &[&str]) = ("wl-paste", &["--no-newline"]);

    let out = std::process::Command::new(cmd).args(args).output().ok()?;
    let _ = std::io::stdout().flush();
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Stable fingerprint for echo suppression of images.
fn hash_bytes(data: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h ^ (data.len() as u64)
}

/// Read the local clipboard image (PNG) if the pasteboard holds one.
fn local_clipboard_image() -> Option<(&'static str, Vec<u8>)> {
    #[cfg(target_os = "macos")]
    {
        let path = std::env::temp_dir().join(format!(
            "ss-clone-read-clip-{}.png",
            std::process::id()
        ));
        let script = format!(
            "try\nset d to (the clipboard as «class PNGf»)\nset f to open for access POSIX file \"{}\" with write permission\nset eof of f to 0\nwrite d to f\nclose access f\non error\nend try",
            path.display()
        );
        let _ = std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .status();
        let data = std::fs::read(&path).ok();
        let _ = std::fs::remove_file(&path);
        let data = data?;
        if data.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Some(("public.png", data));
        }
        None
    }
    #[cfg(not(target_os = "macos"))]
    {
        let out = std::process::Command::new("wl-paste")
            .args(["--type", "image/png"])
            .output()
            .ok()?;
        if out.status.success() && out.stdout.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(("public.png", out.stdout))
        } else {
            None
        }
    }
}

/// Put an image on the local clipboard. The archive already carries encoded
/// bytes (`public.png`/`public.tiff`/`public.jpeg`), so write them to a temp
/// file and let the platform clipboard service take the file's contents.
fn set_local_clipboard_image(uti: &str, data: &[u8]) {
    let (ext, class) = match uti {
        "public.tiff" => ("tiff", "TIFF"),
        "public.jpeg" => ("jpg", "JPEG"),
        _ => ("png", "PNGf"),
    };
    let path = std::env::temp_dir().join(format!(
        "ss-clone-clipboard-{}-{}.{ext}",
        std::process::id(),
        now_ms()
    ));
    if std::fs::write(&path, data).is_err() {
        return;
    }
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "set the clipboard to (read (POSIX file \"{}\") as «class {class}»)",
            path.display()
        );
        // Do not wait here: a blocked osascript (TCC prompt, busy pasteboard)
        // must not stall the thread that pumps framebuffer updates.
        std::thread::spawn(move || {
            let _ = std::process::Command::new("osascript")
                .arg("-e")
                .arg(script)
                .status();
        });
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mime = match uti {
            "public.tiff" => "image/tiff",
            "public.jpeg" => "image/jpeg",
            _ => "image/png",
        };
        use std::io::Write;
        if let Ok(mut child) = std::process::Command::new("wl-copy")
            .arg("--type")
            .arg(mime)
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(data);
            }
            let _ = child.wait();
        }
    }
    let _ = std::fs::remove_file(&path);
}

/// Replace the local text clipboard.
fn set_local_clipboard(text: &str) {
    use std::io::Write;
    #[cfg(target_os = "macos")]
    let (cmd, args): (&str, &[&str]) = ("pbcopy", &[]);
    #[cfg(not(target_os = "macos"))]
    let (cmd, args): (&str, &[&str]) = ("wl-copy", &[]);

    // Never wait on this here: a clipboard subprocess that blocks (a TCC
    // prompt, a busy pasteboard server) must not stall the thread that pumps
    // framebuffer updates. That stall is what made the window look frozen.
    let text = text.to_string();
    std::thread::spawn(move || {
        if let Ok(mut child) = std::process::Command::new(cmd)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    });
}

fn write_png(path: &str, width: u16, height: u16, bgrx: &[u32]) -> Result<()> {
    let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
    for &px in bgrx {
        let b = px as u8;
        let g = (px >> 8) as u8;
        let r = (px >> 16) as u8;
        rgb.push(r);
        rgb.push(g);
        rgb.push(b);
    }
    let file = File::create(path)?;
    let w = BufWriter::new(file);
    let mut enc = png::Encoder::new(w, width as u32, height as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header()?;
    writer.write_image_data(&rgb)?;
    Ok(())
}

/// One dirty region of a decoded frame: geometry plus the pixels of that
/// region only, so the reader never copies the whole framebuffer.
type FrameRect = (u16, u16, u16, u16, Vec<u32>);
/// A decoded frame: geometry, the dirty regions since the last publish, and
/// whether it covered the whole framebuffer.
type Frame = (u16, u16, Vec<FrameRect>, bool);

/// Decodes framebuffer rects and hands the UI the changed region.
///
/// Owned by the session reader thread. Decoding is the expensive part of a
/// frame (an MVS rect can be 12,000 DCT blocks) and must never run on the UI
/// thread, where it starved the event loop and froze the window.
///
/// The decoder keeps the authoritative framebuffer, so partial (incremental)
/// rects accumulate correctly. Each publish copies the union of everything
/// that changed since the UI last took a frame, which means a slow UI skips
/// intermediate publishes without losing content.
struct Decoder {
    fb: Vec<u32>,
    w: u16,
    h: u16,
    mvs: Option<MvsDecoder>,
    /// BGRX canvas the MVS decoder paints into.
    mvs_bytes: Vec<u8>,
    /// Union of the rects decoded since the last publish.
    dirty: Option<(u16, u16, u16, u16)>,
    full: bool,
    debug: bool,
}

impl Decoder {
    fn new(w: u16, h: u16, mvs: bool, debug: bool) -> Self {
        Self {
            fb: vec![0u32; w as usize * h as usize],
            w,
            h,
            mvs: mvs.then(|| MvsDecoder::new(w, h)),
            mvs_bytes: Vec::new(),
            dirty: None,
            full: false,
            debug,
        }
    }

    fn resize(&mut self, w: u16, h: u16) {
        self.w = w;
        self.h = h;
        self.fb = vec![0u32; w as usize * h as usize];
        self.mvs_bytes.clear();
        self.dirty = None;
        if let Some(dec) = self.mvs.as_mut() {
            dec.resize(w, h);
        }
    }

    fn mark(&mut self, x: u16, y: u16, w: u16, h: u16) {
        self.dirty = Some(match self.dirty {
            None => (x, y, w, h),
            Some((x0, y0, w0, h0)) => {
                let nx = x0.min(x);
                let ny = y0.min(y);
                let nx1 = (x0 + w0).max(x + w);
                let ny1 = (y0 + h0).max(y + h);
                (nx, ny, nx1 - nx, ny1 - ny)
            }
        });
        if w == self.w && h == self.h && x == 0 && y == 0 {
            self.full = true;
        }
    }

    fn apply(&mut self, r: &rfb_proto::RectUpdate) {
        if r.encoding == 0x3f3 {
            self.apply_mvs(r);
        } else {
            self.apply_raw(r);
        }
    }

    fn apply_raw(&mut self, r: &rfb_proto::RectUpdate) {
        if r.pixels.is_empty() || r.w == 0 || r.h == 0 {
            return;
        }
        if r.x == 0
            && r.y == 0
            && (r.w > self.w || r.h > self.h)
            && r.pixels.len() == r.w as usize * r.h as usize * 4
        {
            self.resize(r.w, r.h);
        }
        let dst_w = self.w as usize;
        let src_row = r.w as usize * 4;
        let x = r.x as usize;
        let y = r.y as usize;
        for row in 0..r.h as usize {
            let dy = y + row;
            if dy >= self.h as usize || x + r.w as usize > dst_w {
                continue;
            }
            let src = row * src_row;
            if src + src_row > r.pixels.len() {
                break;
            }
            let dst = dy * dst_w + x;
            let out = &mut self.fb[dst..dst + r.w as usize];
            for (px, chunk) in out
                .iter_mut()
                .zip(r.pixels[src..src + src_row].chunks_exact(4))
            {
                *px = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], 0]);
            }
        }
        self.mark(r.x, r.y, r.w, r.h);
    }

    fn apply_mvs(&mut self, r: &rfb_proto::RectUpdate) {
        if self.mvs.is_none() {
            return;
        }
        if r.x == 0 && r.y == 0 && (r.w > self.w || r.h > self.h) {
            self.resize(r.w, r.h);
        }
        let need = self.w as usize * self.h as usize * 4;
        if self.mvs_bytes.len() != need {
            self.mvs_bytes = vec![0u8; need];
        }
        let rect = Rect::new(r.x, r.y, r.w, r.h);
        let debug = self.debug;
        let body_type = r.pixels.first().copied().unwrap_or(255);
        let applied = {
            let Self { mvs, mvs_bytes, .. } = self;
            let Some(dec) = mvs.as_mut() else { return };
            grab::note(&format!("mvs apply {}x{}+{}+{}", r.w, r.h, r.x, r.y));
            match dec.apply(&r.pixels, rect, mvs_bytes) {
                Ok(Some(stats)) => {
                    if debug {
                        if body_type == 1 {
                            eprintln!(
                                "[{:6} ms] t1 {}x{}+{}+{} applied: {} tiles, {} refined",
                                now_ms(),
                                r.w,
                                r.h,
                                r.x,
                                r.y,
                                stats.tiles,
                                stats.dct_blocks
                            );
                        } else {
                            eprintln!(
                                "[{:6} ms] t0 {}x{}+{}+{} applied: {} tiles, {} dct",
                                now_ms(),
                                r.w,
                                r.h,
                                r.x,
                                r.y,
                                stats.tiles,
                                stats.dct_blocks
                            );
                        }
                    }
                    true
                }
                Ok(None) => false,
                Err(e) => {
                    eprintln!("mvs decode {}x{}+{}+{}: {e:#}", r.w, r.h, r.x, r.y);
                    false
                }
            }
        };
        if !applied {
            return;
        }
        // Expand just this rect from the BGRX canvas into the framebuffer.
        let w = self.w as usize;
        let rw = r.w as usize;
        for row in r.y as usize..(r.y as usize + r.h as usize).min(self.h as usize) {
            let start = (row * w + r.x as usize) * 4;
            let end = start + rw * 4;
            if end > self.mvs_bytes.len() {
                break;
            }
            let dst = row * w + r.x as usize;
            let out = &mut self.fb[dst..dst + rw];
            for (px, chunk) in out
                .iter_mut()
                .zip(self.mvs_bytes[start..end].chunks_exact(4))
            {
                *px = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], 0]);
            }
        }
        self.mark(r.x, r.y, r.w, r.h);
    }
}

/// Hand the changed region to the UI. The region is the union of everything
/// changed since the UI last took a frame, so replacing an unconsumed frame
/// never loses content: the next publish covers both.
fn publish_frame(dec: &mut Decoder, slot: &std::sync::Mutex<Option<Frame>>) {
    let Some(mut region) = dec.dirty.take() else {
        return;
    };
    let full = std::mem::take(&mut dec.full);
    let w = dec.w as usize;
    let h = dec.h as usize;
    if let Ok(mut slot) = slot.lock() {
        // Fold in a frame the UI has not taken yet.
        if let Some((_, _, old, old_full)) = slot.as_ref() {
            if let Some((x, y, ow, oh)) = old.first().map(|(x, y, w, h, _)| (*x, *y, *w, *h)) {
                let (x0, y0, x1, y1) = (
                    region.0.min(x),
                    region.1.min(y),
                    (region.0 + region.2).max(x + ow),
                    (region.1 + region.3).max(y + oh),
                );
                region = (x0, y0, x1 - x0, y1 - y0);
            }
            let _ = old_full;
        }
        let (x, y, rw, rh) = region;
        let (x, y, rw, rh) = (x as usize, y as usize, rw as usize, rh as usize);
        if x + rw > w || y + rh > h {
            return;
        }
        let mut pixels = Vec::with_capacity(rw * rh);
        for row in y..y + rh {
            let start = row * w + x;
            pixels.extend_from_slice(&dec.fb[start..start + rw]);
        }
        *slot = Some((dec.w, dec.h, vec![(x as u16, y as u16, rw as u16, rh as u16, pixels)], full));
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn gesture_bodies(g: Gesture, fb_w: u16, fb_h: u16) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    match g {
        Gesture::Swipe {
            motion,
            mask,
            progress,
            phase,
            x,
            y,
            velocity,
            flavor,
            flags,
            ..
        } => {
            if phase == Phase::Ended {
                let px = (x * fb_w as f32).clamp(0.0, fb_w as f32 - 1.0) as u16;
                let py = (y * fb_h as f32).clamp(0.0, fb_h as f32 - 1.0) as u16;
                out.push(gesture_begin_end(1, 0, px, py));
                out.push(gesture_swipe(
                    motion,
                    mask,
                    progress.clamp(0.0, 1.0),
                    x,
                    y,
                    velocity.0,
                    velocity.1,
                    velocity.2,
                    flavor,
                    flags,
                    px,
                    py,
                ));
                out.push(gesture_begin_end(2, 0, px, py));
            }
        }
        Gesture::Scroll { .. } => {
            // Same synthesis as the winit path: the portable gesture model
            // carries pixels, `scroll_message` owns every wire number.
            let (msg, _, _, _) = scroll_message(&g, fb_w / 2, fb_h / 2, (0.0, 0.0));
            if let Some(msg) = msg {
                out.push(msg);
            }
        }
        Gesture::Magnify { factor, phase, x, y } => {
            let px = (x * fb_w as f32).clamp(0.0, fb_w as f32 - 1.0) as u16;
            let py = (y * fb_h as f32).clamp(0.0, fb_h as f32 - 1.0) as u16;
            out.push(gesture_magnify(
                (factor - 1.0) as f64,
                phase.code() as u64,
                0,
                px,
                py,
            ));
        }
        Gesture::Rotate { radians, phase, x, y } => {
            let px = (x * fb_w as f32).clamp(0.0, fb_w as f32 - 1.0) as u16;
            let py = (y * fb_h as f32).clamp(0.0, fb_h as f32 - 1.0) as u16;
            out.push(gesture_rotate(
                radians as f64,
                phase.code() as u64,
                0,
                px,
                py,
            ));
        }
    }
    out
}

#[allow(dead_code)]
fn touch_phase(phase: TouchPhase) -> u32 {
    match phase {
        TouchPhase::Started => 1,
        TouchPhase::Moved => 2,
        TouchPhase::Ended => 4,
        TouchPhase::Cancelled => 8,
    }
}

#[derive(Debug, Clone)]
enum UserEvent {
    Refresh,
    Disconnected(String),
    /// Status text for the window title after a pull attempt.
    PullStatus(String),
    /// A keyboard event captured by the macOS event tap.
    #[cfg(target_os = "macos")]
    Grab(grab::GrabEvent),
}

struct App {
    context: softbuffer::Context<DisplayHandle<'static>>,
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<DisplayHandle<'static>, Arc<Window>>>,
    /// The presented framebuffer. The reader sends dirty regions; the UI only
    /// blits them and presents.
    fb: Vec<u32>,
    /// One-slot handoff: the reader replaces the frame; a newer frame always
    /// wins and a stale one is dropped, never queued.
    frame_slot: Arc<std::sync::Mutex<Option<Frame>>>,
    fb_w: u16,
    fb_h: u16,
    /// Input coordinates stay in the server's original (backing) space even
    /// when the framebuffer image is scaled down.
    input_w: u16,
    input_h: u16,
    tx: Sender<Vec<u8>>,
    /// Event-loop handle for the pull retry thread (window title updates).
    proxy: EventLoopProxy<UserEvent>,
    rx: Receiver<Update>,
    cursor: (u16, u16),
    /// Remote cursor bitmap (0x450 alpha, or -239 mask), once received.
    cursor_shape: Option<CursorUpdate>,
    /// Logical desktop size in points from 0x451. Cursor bitmaps and hotspots
    /// are in points, so the compositor needs this to size them correctly.
    point_size: Option<(u16, u16)>,
    /// Whether the OS pointer is currently over our window (we only draw the
    /// remote cursor then, and only hide the local one when we can replace it).
    pointer_inside: bool,
    /// Set once the local pointer is hidden in favour of the decoded bitmap.
    cursor_hidden: bool,
    /// A decoded remote cursor arrived; hide the OS pointer on the next idle
    /// turn rather than inside the event dispatch.
    want_cursor_hidden: bool,
    /// Remote destination folder for dropped files (`/Users/<user>/Downloads`).
    file_dest: String,
    /// Set while a Cmd+Shift+V pull is waiting for the archive that answers
    /// its fetch(es); cleared by the reader on success or by the retry thread
    /// when it refuses.
    pull_pending: Arc<std::sync::Mutex<Option<PullState>>>,
    /// Last absolute path the remote clipboard carried, used only when a pull
    /// gets no archive at all.
    remote_path: Arc<std::sync::Mutex<Option<String>>>,
    /// The remote's layout table: which key sits where on the far end.
    keymap: &'static Keymap,
    /// macOS keyboard grab, when it could be created.
    grab: Option<GrabHandle>,
    /// The release chord fired: stay ungrabbed until focus is lost and
    /// regained (the user pressed it because something was wrong).
    grab_suspended: bool,
    /// `--demo-cursor`: scripted remote pointer tour to exercise cursor shapes.
    demo_cursor: bool,
    demo_step: usize,
    demo_next: Option<Instant>,
    demo_dump_at: Option<(Instant, String)>,
    buttons: u8,
    dirty: bool,
    modifiers: ModifiersState,
    /// Modifiers the remote has actually been told are down. Shift is excluded:
    /// it is synthesised around shifted characters instead.
    forwarded_mods: ModifiersState,
    xmap: Vec<usize>,
    render_size: (u32, u32),
    surface_size: (u32, u32),
    last_pointer_sent: Instant,
    pending_pointer: Option<(u16, u16, u8)>,
    dump_path: Option<String>,
    dump_after: u32,
    frames: u32,
    full_frame: bool,
    exit_after_dump: bool,
    debug_input: bool,
    ime: bool,
    focused_once: bool,
    deadline: Option<Instant>,
    /// Sub-line scroll remainder, so small pixel deltas still accumulate.
    scroll_rem: (f32, f32),
    /// Scroll deltas coalesced since the last subtype-11 message.
    scroll_pending: Option<PendingScroll>,
    /// When the last scroll message went out (coalescing to ~120 Hz).
    scroll_last: Instant,
    /// `--coast`: forward the OS momentum tail. Default is to drop it, which
    /// is what makes micro-scroll and flick-and-stop stop dead.
    coast: bool,
    /// Scroll events with no momentum classification (monitor missing or a
    /// desync); forwarded and counted so the log shows it.
    scroll_unclassified: u64,
    /// Continuous axis weighting from recent motion (no mode, no latch).
    scroll_axis: AxisFilter,
    /// `--scroll-test stop`: when to send the touch that stops the glide.
    scroll_stop_at: Option<Instant>,
    /// `--scroll-test stress`: remaining scripted flicks and when the next
    /// one starts.
    scroll_stress: u32,
    scroll_stress_next: Option<Instant>,
    /// `--scroll-test`: scripted gesture to exercise the scroll model.
    scroll_test: String,
    /// Open pinch/rotate gesture and when it last saw input.
    pinch_active: bool,
    pinch_last: Option<Instant>,
    rotate_active: bool,
    rotate_last: Option<Instant>,
    pinch_x: u16,
    pinch_y: u16,
    /// `--debug-input` samples of input event -> first painted rectangle.
    lat: Vec<f64>,
    lat_next_report: u32,
}

impl App {
    fn new(
        event_loop: &EventLoop<UserEvent>,
        fb_w: u16,
        fb_h: u16,
        input_w: u16,
        input_h: u16,
        tx: Sender<Vec<u8>>,
        rx: Receiver<Update>,
        proxy: EventLoopProxy<UserEvent>,
        dump_path: Option<String>,
        dump_after: u32,
        exit_after_dump: bool,
        debug_input: bool,
        ime: bool,
        deadline: Option<Instant>,
        demo_cursor: bool,
        file_dest: String,
        pull_pending: Arc<std::sync::Mutex<Option<PullState>>>,
        remote_path: Arc<std::sync::Mutex<Option<String>>>,
        keymap: &'static Keymap,
        grab: Option<GrabHandle>,
        scroll_test: String,
        frame_slot: Arc<std::sync::Mutex<Option<Frame>>>,
        coast: bool,
    ) -> Result<Self> {
        let display = event_loop.display_handle()?;
        // SAFETY: the context is dropped before the event loop (see `exiting`).
        let display = unsafe {
            std::mem::transmute::<DisplayHandle<'_>, DisplayHandle<'static>>(display)
        };
        let context = softbuffer::Context::new(display)
            .map_err(|e| anyhow::anyhow!("softbuffer context: {e:?}"))?;
        Ok(Self {
            context,
            window: None,
            surface: None,
            fb: vec![0u32; fb_w as usize * fb_h as usize],
            frame_slot,
            fb_w,
            fb_h,
            input_w,
            input_h,
            tx,
            proxy,
            rx,
            cursor: (input_w / 2, input_h / 2),
            cursor_shape: None,
            point_size: None,
            pointer_inside: true,
            cursor_hidden: false,
            want_cursor_hidden: false,
            file_dest,
            pull_pending,
            remote_path,
            keymap,
            grab,
            grab_suspended: false,
            scroll_test,
            demo_cursor,
            demo_step: 0,
            demo_next: Some(Instant::now() + Duration::from_millis(600)),
            demo_dump_at: None,
            buttons: 0,
            dirty: true,
            modifiers: ModifiersState::empty(),
            forwarded_mods: ModifiersState::empty(),
            xmap: Vec::new(),
            render_size: (0, 0),
            surface_size: (0, 0),
            last_pointer_sent: Instant::now() - Duration::from_millis(100),
            pending_pointer: None,
            dump_path,
            dump_after,
            frames: 0,
            full_frame: false,
            exit_after_dump,
            debug_input,
            ime,
            focused_once: false,
            deadline,
            scroll_rem: (0.0, 0.0),
            scroll_pending: None,
            scroll_last: Instant::now(),
            coast,
            scroll_unclassified: 0,
            scroll_axis: AxisFilter::new(),
            scroll_stop_at: None,
            scroll_stress: 0,
            scroll_stress_next: None,
            pinch_active: false,
            pinch_last: None,
            rotate_active: false,
            rotate_last: None,
            pinch_x: 0,
            pinch_y: 0,
            lat: Vec::new(),
            lat_next_report: 50,
        })
    }

    fn send(&self, body: Vec<u8>) {
        if self.tx.send(body).is_err() && self.debug_input {
            eprintln!("input channel closed (writer thread exited)");
        }
    }

    /// The one place that decides what a key event puts on the wire. Shared by
    /// winit's `KeyboardInput` and the macOS event tap (`grab.rs`).
    fn handle_key(
        &mut self,
        code: Option<KeyCode>,
        logical: &Key,
        pressed: bool,
        repeat: bool,
    ) {
        if repeat {
            return;
        }
        // Cmd+Shift+V is local: pull the file/folder named on the
        // remote clipboard. Do not forward the V key (Cmd+V alone is
        // still a normal remote paste). Match the physical key so a
        // Japanese/IME layout cannot hide the V.
        let is_v = code == Some(KeyCode::KeyV)
            || matches!(
                logical,
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("v")
            );
        // Cmd+Shift+V is the advertised shortcut; Ctrl+Shift+V is a
        // fallback for setups where another app owns Cmd+Shift+V.
        if is_v
            && self.modifiers.shift_key()
            && (self.modifiers.super_key() || self.modifiers.control_key())
        {
            if pressed {
                self.pull_from_remote();
            }
            return;
        }
        // Test shortcuts for gesture calibration on macOS: Ctrl+Alt+arrows
        // emit a swipe; Ctrl+Alt+=/- emit a magnify.
        if self.modifiers.control_key() && self.modifiers.alt_key() && pressed {
            match logical {
                Key::Named(NamedKey::ArrowLeft) => {
                    self.send_swipe(1, 4);
                    return;
                }
                Key::Named(NamedKey::ArrowRight) => {
                    self.send_swipe(1, 8);
                    return;
                }
                Key::Named(NamedKey::ArrowUp) => {
                    self.send_swipe(2, 1);
                    return;
                }
                Key::Named(NamedKey::ArrowDown) => {
                    self.send_swipe(2, 2);
                    return;
                }
                Key::Character(s) if s.as_str() == "=" || s.as_str() == "+" => {
                    let (x, y) = self.cursor;
                    self.send(gesture_magnify(0.1, 2, 0, x, y));
                    return;
                }
                Key::Character(s) if s.as_str() == "-" => {
                    let (x, y) = self.cursor;
                    self.send(gesture_magnify(-0.1, 2, 0, x, y));
                    return;
                }
                _ => {}
            }
        }
        // With IME enabled, composed text arrives via Ime::Commit.
        if self.ime && matches!(logical, Key::Character(_)) {
            return;
        }
        // If the event reports modifiers we never forwarded (a
        // dropped or reordered ModifiersChanged), put them down before
        // the key and lift them again after it, in reverse order.
        let needed = self.modifiers;
        let synthesized = if pressed {
            needed
                & (ModifiersState::SHIFT
                    | ModifiersState::CONTROL
                    | ModifiersState::ALT
                    | ModifiersState::SUPER)
                & !self.forwarded_mods
        } else {
            ModifiersState::empty()
        };
        if !synthesized.is_empty() {
            self.sync_modifiers(needed);
        }
        // Send the CHARACTER's keysym, not a US physical position:
        // the server's keysym table is built from the *remote* layout
        // (a JIS remote maps '\' to its Yen key and '[' to its own
        // bracket key), so a US-position keysym lands on the wrong key.
        // The server derives the keycode and Shift from the keysym,
        // which is why Shift+2 must arrive as '"' and '@' as '@'.
        match logical {
            // Modifier keys are owned by `sync_modifiers`; winit also
            // reports them as key events, which would double them.
            Key::Named(
                NamedKey::Shift
                | NamedKey::Control
                | NamedKey::Alt
                | NamedKey::Super
                | NamedKey::Meta,
            ) => {}
            Key::Named(named) => {
                if let Some(ks) = named_keysym(named)
                    .or_else(|| code.and_then(|code| self.keymap.special(code)))
                {
                    self.send(key_event(ks, pressed));
                } else if pressed && !repeat {
                    eprintln!(
                        "key: unmapped named {named:?} (physical {:?})",
                        code
                    );
                }
            }
            Key::Character(s) => {
                for c in s.chars() {
                    // TODO(JIS): the daemon has no keysym for the JIS
                    // input keys; they need a keycode-space bypass,
                    // not a guessed keysym.
                    if let Some(code) = code {
                        if is_unsupported_jis_code(code) {
                            if pressed && !repeat {
                                eprintln!(
                                    "key: JIS input key {code:?} has no keysym in the daemon's table; not sent"
                                );
                            }
                            continue;
                        }
                    }
                    // Character first: winit already composed `c`
                    // under the local layout, so ask the target layout
                    // which position and shift level type it. The
                    // server turns that into an ordinary keypress, so
                    // remote IME composition is untouched. A bare
                    // character keysym is only a logged last resort.
                    let ks = character_keysym(
                        self.keymap,
                        code,
                        c,
                        self.modifiers.shift_key(),
                    );
                    let ks = ks.unwrap_or_else(|| {
                        if pressed && !repeat {
                            eprintln!(
                                "key: no {} position for {c:?}; sending the Unicode keysym",
                                self.keymap.name
                            );
                        }
                        text_keysym(c)
                    });
                    self.send(key_event(ks, pressed));
                }
            }
            // Dead keys and unidentified keys have no character; send
            // the physical position so the remote can compose.
            _ => match code.and_then(|code| {
                self.keymap
                    .wire_keysym(code, self.modifiers.shift_key())
            }) {
                Some(ks) => self.send(key_event(ks, pressed)),
                None => {
                    if pressed && !repeat {
                        eprintln!(
                            "key: unmapped physical {:?} (logical {:?})",
                            code, logical
                        );
                    }
                }
            },
        }
        if !synthesized.is_empty() {
            self.sync_modifiers(needed - synthesized);
        }
    }

    /// Make the remote's modifier state match `target`.
    ///
    /// Shift is a real modifier now that keys are physical: the remote layout
    /// decides the shifted symbol. Downs go out in a fixed order, ups in
    /// reverse, and the tracked state is only advanced for what was actually
    /// sent, so a missed or reordered `ModifiersChanged` can never leave a
    /// modifier stuck down.
    fn sync_modifiers(&mut self, target: ModifiersState) {
        // Option is Meta_L (0xFFE7) on this server: Alt_L (0xFFE9) is accepted
        // as a key but never raises the Option flag, which silently turned
        // Option+Cmd+C into a plain Cmd+C.
        let bits = [
            (ModifiersState::SHIFT, 0xffe1u32),
            (ModifiersState::CONTROL, 0xffe3),
            (ModifiersState::ALT, 0xffe7),
            (ModifiersState::SUPER, 0xffeb),
        ];
        for (bit, keysym) in bits {
            if target.contains(bit) && !self.forwarded_mods.contains(bit) {
                self.send(key_event(keysym, true));
                self.forwarded_mods.insert(bit);
            }
        }
        for (bit, keysym) in bits.iter().rev() {
            if !target.contains(*bit) && self.forwarded_mods.contains(*bit) {
                self.send(key_event(*keysym, false));
                self.forwarded_mods.remove(*bit);
            }
        }
    }

    /// Cmd+Shift+V: pull the file/folder named on the remote clipboard.
    ///
    /// The server's clipboard agent may still be updating the pasteboard when
    /// the first fetch lands, so two more fetches go out ~180 ms apart. The
    /// reader resolves a path as soon as one appears; if none does within
    /// `PULL_WINDOW`, the retry thread refuses and quotes what it saw.
    fn pull_from_remote(&mut self) {
        {
            let mut pending = match self.pull_pending.lock() {
                Ok(p) => p,
                Err(_) => return,
            };
            if let Some(state) = pending.as_ref() {
                if state.started.elapsed() < PULL_WINDOW {
                    eprintln!("pull-from-remote: already waiting for the clipboard");
                    return;
                }
                eprintln!("pull-from-remote: previous attempt timed out; starting over");
            }
            *pending = Some(PullState {
                started: Instant::now(),
                attempts: 1,
                last_seen: None,
            });
        }
        eprintln!(
            "pull-from-remote: Cmd/Ctrl+Shift+V -> fetch 1/{PULL_FETCHES}, waiting for its archive"
        );
        if let Some(window) = &self.window {
            window.set_title("ss-clone — pull: reading remote clipboard…");
        }
        self.send(clipboard_fetch());

        let pending = self.pull_pending.clone();
        let remote_path = self.remote_path.clone();
        let tx = self.tx.clone();
        let proxy = self.proxy.clone();
        thread::spawn(move || {
            for _ in 1..PULL_FETCHES {
                thread::sleep(PULL_RETRY);
                let attempt = match pending.lock() {
                    Ok(mut p) => match p.as_mut() {
                        Some(state) => {
                            state.attempts += 1;
                            Some(state.attempts)
                        }
                        None => None,
                    },
                    Err(_) => None,
                };
                let Some(attempt) = attempt else { return };
                eprintln!(
                    "pull-from-remote: no path yet, retrying the fetch ({attempt}/{PULL_FETCHES})"
                );
                if tx.send(clipboard_fetch()).is_err() {
                    return;
                }
            }
            // Give the last response the rest of the window to land.
            thread::sleep(PULL_RETRY);
            let state = match pending.lock() {
                Ok(mut p) => p.take(),
                Err(_) => None,
            };
            let Some(state) = state else { return };
            if state.last_seen.is_none() {
                let cached = remote_path.lock().ok().and_then(|p| p.clone());
                if let Some(cached) = cached {
                    let session = filecopy::next_session();
                    eprintln!(
                        "pull-from-remote: stage=cached-path {cached} (session {session}); no archive arrived after {} fetches",
                        state.attempts
                    );
                    let name = cached.rsplit('/').next().unwrap_or("file");
                    let _ = proxy.send_event(UserEvent::PullStatus(format!(
                        "ss-clone — pulling {name}…"
                    )));
                    let _ = tx.send(rfb_proto::filecopy::start_file_send(&cached, session));
                    return;
                }
            }
            let seen = state
                .last_seen
                .unwrap_or_else(|| "no clipboard archive arrived".to_string());
            eprintln!(
                "pull-from-remote: {seen} after {} fetches in {} ms - on the remote, put the path on the clipboard as TEXT (e.g. copy it from a Terminal), then press Cmd+Shift+V again; or drop the file on this window.",
                state.attempts,
                state.started.elapsed().as_millis()
            );
            let _ = proxy.send_event(UserEvent::PullStatus(
                "ss-clone — No path on the remote clipboard - on the remote, put the path on the clipboard as TEXT (e.g. copy it from a Terminal), then press Cmd+Shift+V again; or drop the file on this window."
                    .to_string(),
            ));
        });
    }

    /// Coalesce pointer motion to at most ~125 Hz; buttons always go out now.
    fn flush_pointer(&mut self, force: bool) {
        if !force && self.last_pointer_sent.elapsed() < Duration::from_millis(8) {
            return;
        }
        if let Some((x, y, mask)) = self.pending_pointer.take() {
            self.send(pointer_event(mask, x, y));
            self.last_pointer_sent = Instant::now();
        }
    }

    /// Queue a two-finger scroll. macOS acts on the 16-bit line delta in the
    /// Apple MouseScroll message (subtype 11); the pixel deltas are ignored by
    /// this server build, and phase 2 alone is sufficient (VERIFIED live:
    /// `dy=-10` moved TextEdit 15 lines, `dy=+10` moved it back 8).
    /// Queue one scroll sample from the portable gesture model. Every sample
    /// is eventually sent: the point deltas carry the smooth motion, the
    /// integer line deltas are what this server honours today, and
    /// Began/Ended/Cancelled are never coalesced away.
    fn queue_scroll(&mut self, gesture: Gesture) {
        let Gesture::Scroll {
            dx,
            dy,
            phase,
            precise,
            momentum,
        } = gesture
        else {
            return;
        };
        let now = Instant::now();
        if phase == Phase::Began {
            self.scroll_axis.begin();
        }
        // Continuous axis weighting: the minor axis is scaled by how much of
        // the recent motion it represents, so jitter disappears while a
        // diagonal or a direction change still works.
        let (dx, dy) = self.scroll_axis.apply(dx, dy, now);
        if self.debug_input {
            if let Some(t) = self.scroll_axis.take_transition() {
                eprintln!(
                    "scroll axis {} -> {} (ratio {:.2}, minor {:.1} pt/event, dwell {} ms)",
                    t.from.name(),
                    t.to.name(),
                    t.ratio,
                    t.minor_ema,
                    t.dwell_ms
                );
            }
        }
        let pending = self.scroll_pending.get_or_insert(PendingScroll {
            dx: 0.0,
            dy: 0.0,
            phase,
            momentum,
            precise,
            raw: (0.0, 0.0),
        });
        pending.dx += dx;
        pending.dy += dy;
        pending.phase = phase;
        pending.momentum = momentum;
        pending.precise = precise;
        pending.raw = (dx, dy);

        // Began/Ended/Cancelled go out immediately; Changed coalesces to
        // ~120 Hz so a fast flick does not flood the session.
        let urgent = !matches!(phase, Phase::Changed);
        if urgent || self.scroll_last.elapsed() >= SCROLL_INTERVAL {
            self.flush_scroll();
        }
    }

    /// Send the coalesced scroll sample as one subtype-11 message.
    fn flush_scroll(&mut self) {
        let Some(pending) = self.scroll_pending.take() else {
            return;
        };
        self.scroll_last = Instant::now();
        let gesture = Gesture::Scroll {
            dx: pending.dx,
            dy: pending.dy,
            phase: pending.phase,
            precise: pending.precise,
            momentum: pending.momentum,
        };
        self.emit_scroll(&gesture, false);
    }

    /// Synthesise and send one scroll message through the portable model.
    ///
    /// The log shows the fractional accumulator and the integer lines that
    /// actually went on the wire, so dropped sub-line motion is visible.
    fn emit_scroll(&mut self, gesture: &Gesture, from_momentum: bool) {
        let (x, y) = self.cursor;
        let (msg, remainder, lines, fixed) =
            scroll_message(gesture, x, y, self.scroll_rem);
        self.scroll_rem = remainder;
        if self.debug_input {
            if let Gesture::Scroll {
                dx,
                dy,
                phase,
                precise,
                ..
            } = *gesture
            {
                eprintln!(
                    "scroll t={} ms raw=({dx:.1},{dy:.1}) point=({},{}) lines=({},{}) fixed=({},{}) phase={} precise={} momentum={from_momentum} speed={:.2}",
                    START.get_or_init(Instant::now).elapsed().as_millis(),
                    dx.round() as i32,
                    dy.round() as i32,
                    lines.0,
                    lines.1,
                    fixed.0,
                    fixed.1,
                    phase.code(),
                    precise,
                    scroll_speed()
                );
            }
        }
        if let Some(msg) = msg {
            self.send(msg);
        }
    }

    fn send_swipe(&self, motion: u32, mask: u32) {        let (x, y) = self.cursor;
        self.send(gesture_begin_end(1, 0, x, y));
        self.send(gesture_swipe(
            motion, mask, 1.0, 0.5, 0.5, 0.0, 0.0, 0.0, 0, 0, x, y,
        ));
        self.send(gesture_begin_end(2, 0, x, y));
    }

    fn to_fb(&self, pos: PhysicalPosition<f64>) -> (u16, u16) {
        let (ww, wh) = self
            .window
            .as_ref()
            .map(|w| {
                let s = w.inner_size();
                (s.width.max(1), s.height.max(1))
            })
            .unwrap_or((1, 1));
        let x = (pos.x * self.input_w as f64 / ww as f64)
            .clamp(0.0, self.input_w as f64 - 1.0) as u16;
        let y = (pos.y * self.input_h as f64 / wh as f64)
            .clamp(0.0, self.input_h as f64 - 1.0) as u16;
        (x, y)
    }

    /// Decode an MVS (0x3f3) rectangle into `self.fb`.
    fn record_input_latency(&mut self) {
        if !self.debug_input {
            return;
        }
        let pending = INPUT_PENDING_MS.swap(u64::MAX, Ordering::Relaxed);
        if pending != u64::MAX {
            let ms = now_ms().saturating_sub(pending) as f64;
            self.lat.push(ms);
            eprintln!("[{:6} ms] input->applied {ms:.1} ms", now_ms());
        }
    }

    fn report_input_latency(&self, tag: &str) {
        if !self.debug_input {
            return;
        }
        let mut v = self.lat.clone();
        v.sort_by(f64::total_cmp);
        let p50 = v.get(v.len() / 2).copied().unwrap_or(f64::NAN);
        let max = v.last().copied().unwrap_or(f64::NAN);
        eprintln!(
            "[{:6} ms] {tag}: input->applied p50 {p50:.1} ms, max {max:.1} ms (n={})",
            now_ms(),
            v.len()
        );
    }

    fn apply_update(&mut self, update: Update) {
        if let Some(d) = update.display {
            self.point_size = Some((d.point_width, d.point_height));
            if self.debug_input {
                eprintln!(
                    "display layout: {}x{} points / {}x{} pixels",
                    d.point_width, d.point_height, d.pixel_width, d.pixel_height
                );
            }
        }
        if let Some((x, y)) = update.cursor_position {
            if (x, y) != self.cursor {
                eprintln!("remote pointer position ({x},{y})");
                self.cursor = (x, y);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
        }
        if let Some(c) = update.cursor {
            // Cursor bitmaps are sized in remote *points* (0x451). The
            // on-screen size is points * window_px / point_px, which at 2x
            // backing doubles the bitmap relative to the framebuffer.
            if self.debug_input {
                let (ww, wh) = self
                    .window
                    .as_ref()
                    .map(|w| {
                        let s = w.inner_size();
                        (s.width.max(1), s.height.max(1))
                    })
                    .unwrap_or((self.fb_w as u32, self.fb_h as u32));
                let (ox, oy, sx, sy) = Self::cursor_box(
                    &c,
                    self.cursor,
                    self.input_w,
                    self.input_h,
                    self.point_size,
                    ww,
                    wh,
                );
                eprintln!(
                    "cursor change: bitmap={}x{} points hotspot=({},{}) scale={sx:.2}x{sy:.2} drawn={:.0}x{:.0} px at ({:.0},{:.0})",
                    c.width,
                    c.height,
                    c.hotspot.0,
                    c.hotspot.1,
                    c.width as f64 * sx,
                    c.height as f64 * sy,
                    ox,
                    oy
                );
            } else if let Some(m) = &c.mask {
                eprintln!(
                    "remote cursor: {}x{} hotspot=({},{}) RichCursor mask={} bytes",
                    c.width,
                    c.height,
                    c.hotspot.0,
                    c.hotspot.1,
                    m.len()
                );
            } else {
                eprintln!(
                    "remote cursor: {}x{} hotspot=({},{}) alpha plane",
                    c.width, c.height, c.hotspot.0, c.hotspot.1
                );
            }
            self.cursor_shape = Some(c);
            // Defer the actual OS call: `set_cursor_visible(false)` makes winit
            // call `invalidateCursorRectsForView` on the window, and doing that
            // from inside winit's own event dispatch is re-entrant AppKit
            // usage. It is applied in `about_to_wait` instead, which is outside
            // the dispatch and is where every other window mutation belongs.
            self.want_cursor_hidden = true;
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
    }

    fn draw(&mut self) -> Result<()> {
        let started = Instant::now();
        let Some(window) = self.window.clone() else {
            return Ok(());
        };
        let size = window.inner_size();
        let (w, h) = (size.width, size.height);
        if w == 0 || h == 0 {
            return Ok(());
        }
        let Some(surface) = self.surface.as_mut() else {
            return Ok(());
        };
        if self.surface_size != (w, h) {
            surface
                .resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap())
                .map_err(|e| anyhow::anyhow!("softbuffer resize: {e:?}"))?;
            self.surface_size = (w, h);
        }
        if self.render_size != (w, h) {
            self.xmap = (0..w as usize)
                .map(|x| (x as u64 * self.fb_w as u64 / w as u64) as usize)
                .collect();
            self.render_size = (w, h);
        }
        let mut buffer = surface
            .buffer_mut()
            .map_err(|e| anyhow::anyhow!("softbuffer buffer: {e:?}"))?;

        if self.fb_w as u32 == w && self.fb_h as u32 == h {
            // 1:1: one memcpy for the whole frame.
            buffer.copy_from_slice(&self.fb[..w as usize * h as usize]);
        } else {
            let fw = self.fb_w as usize;
            let fh = self.fb_h as u64;
            // Row-copy fast path when the x mapping is the identity (the usual
            // "window is the same width, slightly different height" case).
            let identity_x = w == self.fb_w as u32;
            for y in 0..h as usize {
                let sy = (y as u64 * fh / h as u64) as usize;
                let row = &self.fb[sy * fw..sy * fw + fw];
                let out = &mut buffer[y * w as usize..(y + 1) * w as usize];
                if identity_x {
                    out.copy_from_slice(row);
                } else {
                    for (x, dst) in out.iter_mut().enumerate() {
                        *dst = row[self.xmap[x]];
                    }
                }
            }
        }
        Self::composite_cursor(
            &self.cursor_shape,
            self.cursor,
            self.input_w,
            self.input_h,
            self.point_size,
            self.pointer_inside,
            &mut buffer,
            w,
            h,
        );
        if self.debug_input {
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            if ms > 5.0 {
                eprintln!("draw took {ms:.1} ms ({}x{} -> {}x{})", self.fb_w, self.fb_h, w, h);
            }
        }
        window.pre_present_notify();
        buffer
            .present()
            .map_err(|e| anyhow::anyhow!("softbuffer present: {e:?}"))?;
        self.dirty = false;
        Ok(())
    }

    /// Cursor geometry in the target buffer: top-left origin plus per-axis
    /// scale from cursor points to target pixels. Pointer position is in
    /// framebuffer pixels; bitmap size/hotspot are in remote points (0x451).
    fn cursor_box(
        c: &CursorUpdate,
        cursor: (u16, u16),
        input_w: u16,
        input_h: u16,
        point_size: Option<(u16, u16)>,
        w: u32,
        h: u32,
    ) -> (f64, f64, f64, f64) {
        let (kx, ky) = (w as f64 / input_w.max(1) as f64, h as f64 / input_h.max(1) as f64);
        let (sx, sy) = point_size
            .map(|(pw, ph)| {
                (
                    w as f64 / pw.max(1) as f64,
                    h as f64 / ph.max(1) as f64,
                )
            })
            .unwrap_or((kx, ky));
        (
            cursor.0 as f64 * kx - c.hotspot.0 as f64 * sx,
            cursor.1 as f64 * ky - c.hotspot.1 as f64 * sy,
            sx,
            sy,
        )
    }

    /// Composite the decoded remote cursor bitmap over the presented frame.
    ///
    /// The remote pointer position is our own last motion (in server backing
    /// space), mapped to window pixels with the same factor as `to_fb`, and the
    /// bitmap is offset by its hotspot. Nearest-neighbour keeps this cheap; the
    /// shape comes straight from the server, so an I-beam or resize cursor is
    /// exactly what macOS drew.
    fn composite_cursor(
        shape: &Option<CursorUpdate>,
        cursor: (u16, u16),
        input_w: u16,
        input_h: u16,
        point_size: Option<(u16, u16)>,
        pointer_inside: bool,
        buffer: &mut [u32],
        w: u32,
        h: u32,
    ) {
        if !pointer_inside {
            return;
        }
        let Some(c) = shape else { return };
        if c.width == 0 || c.height == 0 || input_w == 0 || input_h == 0 {
            return;
        }
        let npx = c.width as usize * c.height as usize;
        if c.rgba.len() < npx * 4 {
            return;
        }
        // 0x450 carries a straight-alpha plane; RichCursor (-239) carries a
        // 1-bit row-padded mask instead.
        let stride = (c.width as usize + 7) / 8;
        let mask = c.mask.as_deref().filter(|m| m.len() >= stride * c.height as usize);
        // Pointer position is in framebuffer pixels; bitmap size and hotspot
        // are in remote *points* (0x451), so the cursor's on-screen size is
        // points * target_px / point_px while its anchor stays in the
        // framebuffer mapping.
        let (ox, oy, sx, sy) =
            Self::cursor_box(c, cursor, input_w, input_h, point_size, w, h);
        let x0 = ox.floor().max(0.0) as i64;
        let y0 = oy.floor().max(0.0) as i64;
        let x1 = ((ox + c.width as f64 * sx).ceil() as i64).min(w as i64);
        let y1 = ((oy + c.height as f64 * sy).ceil() as i64).min(h as i64);
        let win_w = w as usize;
        for wy in y0..y1 {
            let cy = (((wy as f64 - oy) / sy) as usize).min(c.height as usize - 1);
            let prow = cy * c.width as usize;
            for wx in x0..x1 {
                let cx = (((wx as f64 - ox) / sx) as usize).min(c.width as usize - 1);
                let i = (prow + cx) * 4;
                let alpha = match mask {
                    Some(m) => {
                        if (m[cy * stride + cx / 8] >> (7 - (cx % 8))) & 1 == 0 {
                            continue;
                        }
                        255
                    }
                    None => c.rgba[i + 3],
                };
                if alpha == 0 {
                    continue;
                }
                let (r, g, b) = (c.rgba[i] as u32, c.rgba[i + 1] as u32, c.rgba[i + 2] as u32);
                let di = wy as usize * win_w + wx as usize;
                buffer[di] = if alpha == 255 {
                    (r << 16) | (g << 8) | b
                } else {
                    // Straight-alpha blend over the frame.
                    let a = alpha as u32;
                    let dst = buffer[di];
                    let dr = (dst >> 16) & 0xff;
                    let dg = (dst >> 8) & 0xff;
                    let db = dst & 0xff;
                    let mix = |s: u32, d: u32| ((s * a + d * (255 - a) + 127) / 255) & 0xff;
                    (mix(r, dr) << 16) | (mix(g, dg) << 8) | mix(b, db)
                };
            }
        }
    }

    /// The framebuffer with the remote cursor composited, for `--dump` output.
    fn dump_frame(&self) -> Vec<u32> {
        if self.debug_input {
            if let Some(c) = &self.cursor_shape {
                let (ox, oy, sx, sy) = Self::cursor_box(
                    c,
                    self.cursor,
                    self.input_w,
                    self.input_h,
                    self.point_size,
                    self.fb_w as u32,
                    self.fb_h as u32,
                );
                eprintln!(
                    "dump cursor: {}x{} points at ({:.0},{:.0}) drawn={:.0}x{:.0} px (scale {sx:.2}x{sy:.2})",
                    c.width,
                    c.height,
                    ox,
                    oy,
                    c.width as f64 * sx,
                    c.height as f64 * sy
                );
            }
        }
        let mut fb = self.fb.clone();
        Self::composite_cursor(
            &self.cursor_shape,
            self.cursor,
            self.input_w,
            self.input_h,
            self.point_size,
            true,
            &mut fb,
            self.fb_w as u32,
            self.fb_h as u32,
        );
        fb
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Refresh => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            UserEvent::Disconnected(reason) => {
                eprintln!("session disconnected: {reason}");
                if let Some(w) = &self.window {
                    w.set_title("ss-clone — disconnected");
                }
            }
            UserEvent::PullStatus(status) => {
                eprintln!("window title: {status}");
                if let Some(w) = &self.window {
                    w.set_title(&status);
                }
            }
            #[cfg(target_os = "macos")]
            UserEvent::Grab(event) => match event {
                grab::GrabEvent::Key {
                    keycode,
                    chars,
                    flags,
                    pressed,
                    repeat,
                    modifier_only,
                } => {
                    self.modifiers = grab::modifiers_from_flags(flags);
                    let target = self.modifiers;
                    self.sync_modifiers(target);
                    if modifier_only {
                        return;
                    }
                    note_input();
                    let code = grab::keycode_to_winit(keycode);
                    let logical = grab::logical_for(keycode, chars.as_deref());
                    if self.debug_input {
                        eprintln!(
                            "tap key {:?} code={code:?} logical={logical:?} mods={:?}",
                            if pressed { "down" } else { "up" },
                            self.modifiers
                        );
                    }
                    self.handle_key(code, &logical, pressed, repeat);
                }
                grab::GrabEvent::Released { held } => {
                    self.grab_suspended = true;
                    eprintln!(
                        "grab: released by the chord after {} ms; local shortcuts are back until the window loses and regains focus",
                        held.as_millis()
                    );
                    self.modifiers = ModifiersState::empty();
                    self.sync_modifiers(ModifiersState::empty());
                    if let Some(w) = &self.window {
                        w.set_title("ss-clone — grab released — click away to re-arm");
                    }
                }
                grab::GrabEvent::HoldCancelled { held } => {
                    eprintln!(
                        "grab: release chord cancelled after {} ms (another key went down)",
                        held.as_millis()
                    );
                }
                grab::GrabEvent::Reenabled { reason } => {
                    eprintln!("grab: macOS disabled the event tap ({reason}); re-enabled it");
                }
            },
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        eprintln!("resumed");
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("ss-clone — Apple Screen Sharing")
            .with_inner_size(LogicalSize::new(1280.0, 800.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        let surface = softbuffer::Surface::new(&self.context, window.clone())
            .unwrap_or_else(|e| panic!("softbuffer surface: {e:?}"));
        self.surface = Some(surface);
        self.window = Some(window);
        if let Some(w) = &self.window {
            w.set_ime_allowed(self.ime);
            w.set_visible(true);
            w.focus_window();
            w.request_redraw();
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(_) => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                if let Err(e) = self.draw() {
                    eprintln!("draw error: {e:#}");
                }
            }
            WindowEvent::Focused(focused) => {
                if self.debug_input {
                    eprintln!("window focused={focused}");
                }
                if let Some(grab) = &self.grab {
                    if focused && !self.grab_suspended {
                        grab.set_enabled(true);
                        eprintln!("grab: enabled (window focused)");
                    } else if !focused {
                        grab.set_enabled(false);
                        eprintln!("grab: disabled (window lost focus)");
                    }
                }
                if !focused {
                    // Re-arm the chord on the next focus, and never leave a
                    // modifier down on the remote when focus leaves us.
                    self.grab_suspended = false;
                    self.modifiers = ModifiersState::empty();
                    self.sync_modifiers(ModifiersState::empty());
                }
                if let Some(w) = &self.window {
                    w.set_title(if focused {
                        "ss-clone — Apple Screen Sharing"
                    } else {
                        "ss-clone — click to focus"
                    });
                }
            }
            WindowEvent::Ime(Ime::Enabled) => {
                if self.debug_input {
                    eprintln!("IME enabled");
                }
            }
            WindowEvent::Ime(Ime::Disabled) => {
                if self.debug_input {
                    eprintln!("IME disabled");
                }
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
                if self.debug_input {
                    eprintln!("IME commit {text:?}");
                }
                for c in text.chars() {
                    let ks = text_keysym(c);
                    self.send(key_event(ks, true));
                    self.send(key_event(ks, false));
                }
            }
            WindowEvent::Ime(Ime::Preedit(text, _)) => {
                if self.debug_input {
                    eprintln!("IME preedit {text:?}");
                }
            }
            WindowEvent::DroppedFile(path) => {
                note_input();
                if let Some(window) = &self.window {
                    window.set_title("ss-clone — Apple Screen Sharing");
                }
                let debug = self.debug_input;
                let destination = self.file_dest.clone();
                let tx = self.tx.clone();
                // Read/enumerate/chunk off the event loop; the UI only logs.
                thread::spawn(move || {
                    if let Err(error) =
                        filecopy::send_file(tx, path.clone(), destination, debug)
                    {
                        eprintln!("filecopy: drop {} failed: {error:#}", path.display());
                    }
                });
            }
            WindowEvent::HoveredFile(path) => {
                if let Some(window) = &self.window {
                    let name = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("file");
                    window.set_title(&format!("ss-clone — drop {name} to send"));
                }
            }
            WindowEvent::HoveredFileCancelled => {
                if let Some(window) = &self.window {
                    window.set_title("ss-clone — Apple Screen Sharing");
                }
            }
            WindowEvent::CursorEntered { .. } => {
                self.pointer_inside = true;
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::CursorLeft { .. } => {
                self.pointer_inside = false;
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                note_input();
                let (x, y) = self.to_fb(position);
                if self.debug_input && self.buttons != 0 {
                    eprintln!("cursor fb=({x},{y}) buttons={}", self.buttons);
                }
                self.pointer_inside = true;
                if (x, y) != self.cursor {
                    self.cursor = (x, y);
                    self.pending_pointer = Some((x, y, self.buttons));
                    self.flush_pointer(false);
                    if self.cursor_shape.is_some() {
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                note_input();
                if state == ElementState::Pressed {
                    FOCUS_Y.store(self.cursor.1 as u64, Ordering::Relaxed);
                }
                if self.debug_input {
                    eprintln!("mouse {button:?} {state:?}");
                }
                if state == ElementState::Pressed {
                    if let Some(w) = &self.window {
                        w.focus_window();
                    }
                }
                let bit = match button {
                    MouseButton::Left => 1,
                    MouseButton::Right => 2,
                    MouseButton::Middle => 4,
                    MouseButton::Back => 8,
                    MouseButton::Forward => 16,
                    MouseButton::Other(_) => 0,
                };
                if state == ElementState::Pressed {
                    self.buttons |= bit;
                } else {
                    self.buttons &= !bit;
                }
                let (x, y) = self.cursor;
                if self.debug_input {
                    let ms = START.get_or_init(Instant::now).elapsed().as_millis() as u64;
                    eprintln!("[{ms:6} ms] mouse {button:?} {state:?} at ({x},{y})");
                }
                self.pending_pointer = Some((x, y, self.buttons));
                self.flush_pointer(true);
            }
            WindowEvent::MouseWheel { delta, phase, .. } => {
                note_input();
                // Trackpads report pixel deltas (and a real phase) on macOS;
                // a wheel reports lines. Both go out as subtype 11: the point
                // deltas carry the smooth motion, the line fields keep the
                // behaviour this server honours today.
                let (dx, dy, precise) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => {
                        (x * PIXELS_PER_LINE, y * PIXELS_PER_LINE, false)
                    }
                    // winit passes NSEvent.scrollingDeltaX/Y through unchanged,
                    // which is already in logical points - do NOT divide by the
                    // Retina scale factor or trackpad scrolling runs at half
                    // speed (and, with the old 120 pt/line constant, at 1/24th).
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32, p.y as f32, true),
                };
                // winit cannot tell the OS momentum tail from a new touch (it
                // maps momentumPhase onto the same TouchPhase and gives it
                // priority), so the macOS NSEvent monitor classifies each
                // event before dispatch. Touchdown events always go through;
                // momentum events are dropped unless `--coast` asked for them.
                #[cfg(target_os = "macos")]
                let is_momentum = momentum::take();
                #[cfg(target_os = "macos")]
                if self.debug_input {
                    let queued = momentum::pending();
                    if queued > 4 {
                        eprintln!("scroll: classification queue {queued} deep (monitor/winit desync)");
                    }
                }
                #[cfg(not(target_os = "macos"))]
                let is_momentum: Option<bool> = None;
                if self.debug_input {
                    match is_momentum {
                        Some(true) => eprintln!(
                            "scroll: momentum event ({})",
                            if self.coast { "forwarded" } else { "dropped" }
                        ),
                        Some(false) => eprintln!("scroll: touch event"),
                        None => {
                            self.scroll_unclassified += 1;
                            eprintln!(
                                "scroll: unclassified event (#{}), forwarding",
                                self.scroll_unclassified
                            );
                        }
                    }
                }
                if !scroll_forward(is_momentum, self.coast) {
                    return;
                }
                self.queue_scroll(Gesture::Scroll {
                    dx,
                    dy,
                    phase: phase_from_touch(phase),
                    precise,
                    momentum: None,
                });
            }
            WindowEvent::PinchGesture { delta, .. } => {
                note_input();
                // VERIFIED shape: begin(1) -> change(2) with mask 31 -> end(4).
                let (x, y) = self.cursor;
                self.pinch_x = x;
                self.pinch_y = y;
                if !self.pinch_active {
                    self.send(gesture_magnify(0.0, 1, 31, x, y));
                    self.pinch_active = true;
                }
                self.send(gesture_magnify(delta, 2, 31, x, y));
                self.pinch_last = Some(Instant::now());
            }
            WindowEvent::RotationGesture { delta, .. } => {
                note_input();
                let (x, y) = self.cursor;
                self.pinch_x = x;
                self.pinch_y = y;
                if !self.rotate_active {
                    self.send(gesture_rotate(0.0, 1, 31, x, y));
                    self.rotate_active = true;
                }
                self.send(gesture_rotate(delta as f64, 2, 31, x, y));
                self.rotate_last = Some(Instant::now());
            }
            WindowEvent::DoubleTapGesture { .. } => {
                let (x, y) = self.cursor;
                self.send(gesture_smart_magnify(x, y));
            }
            WindowEvent::PanGesture { delta, .. } => {
                note_input();
                // Two-finger pan is scrolling: use the verified subtype 11.
                self.queue_scroll(Gesture::Scroll {
                    dx: delta.x,
                    dy: delta.y,
                    phase: Phase::Changed,
                    precise: true,
                    momentum: None,
                });
            }
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
                // Command/Control/Option go out as real modifier key events.
                // Shift is synthesised around shifted characters below, so it
                // is deliberately not forwarded here (that would double it).
                let target = self.modifiers;
                self.sync_modifiers(target);
            }
            WindowEvent::KeyboardInput { event, .. } => {
                note_input();
                if self.debug_input {
                    eprintln!(
                        "key {:?} physical={:?} logical={:?} text={:?} mods={:?}",
                        event.state, event.physical_key, event.logical_key, event.text, self.modifiers
                    );
                }
                let code = match event.physical_key {
                    PhysicalKey::Code(code) => Some(code),
                    PhysicalKey::Unidentified(_) => None,
                };
                self.handle_key(
                    code,
                    &event.logical_key,
                    event.state == ElementState::Pressed,
                    event.repeat,
                );
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        grab::beat_ui();
        if self.want_cursor_hidden && !self.cursor_hidden {
            if let Some(w) = &self.window {
                w.set_cursor_visible(false);
                self.cursor_hidden = true;
            }
            self.want_cursor_hidden = false;
        }
        if !self.focused_once {
            if let Some(w) = &self.window {
                w.focus_window();
            }
            self.focused_once = true;
        }
        self.flush_pointer(false);
        if self.demo_cursor {
            let mut wake: Option<Instant> = None;
            let now = Instant::now();
            if let Some((at, path)) = self.demo_dump_at.clone() {
                if now >= at {
                    self.demo_dump_at = None;
                    let frame = self.dump_frame();
                    match write_png(&path, self.fb_w, self.fb_h, &frame) {
                        Ok(()) => eprintln!("cursor demo: wrote {path}"),
                        Err(e) => eprintln!("cursor demo: dump {path} failed: {e:#}"),
                    }
                } else {
                    wake = Some(at);
                }
            }
            if let Some(next) = self.demo_next {
                if now >= next {
                    const STOPS: [(u16, u16, &str); 4] = [
                        (500, 350, "textedit text"),
                        (300, 600, "terminal text"),
                        (2168, 600, "terminal right edge"),
                        (60, 120, "desktop"),
                    ];
                    let (x, y, what) = STOPS[self.demo_step.min(STOPS.len() - 1)];
                    self.cursor = (x, y);
                    self.send(pointer_event(0, x, y));
                    let path = format!("artifacts/cursor-gui-{}.png", self.demo_step + 1);
                    eprintln!(
                        "cursor demo: pointer -> ({x},{y}) [{what}], dumping {path} in 1200 ms"
                    );
                    let at = now + Duration::from_millis(1200);
                    self.demo_dump_at = Some((at, path));
                    self.demo_step += 1;
                    self.demo_next = if self.demo_step < STOPS.len() {
                        Some(now + Duration::from_millis(2200))
                    } else {
                        None
                    };
                    wake = Some(at);
                } else {
                    wake = Some(wake.map_or(next, |w| w.min(next)));
                }
            }
            if let Some(at) = wake {
                event_loop.set_control_flow(ControlFlow::WaitUntil(at));
            }
        }
        if self.pinch_active
            && self.pinch_last.map_or(true, |t| t.elapsed() > Duration::from_millis(150))
        {
            self.send(gesture_magnify(0.0, 4, 31, self.pinch_x, self.pinch_y));
            self.pinch_active = false;
        }
        // The post-release glide: one line per step at a decaying rate.
        let now = Instant::now();
        if !self.scroll_test.is_empty()
            && now >= *START.get_or_init(Instant::now) + Duration::from_secs(3)
        {
            let kind = std::mem::take(&mut self.scroll_test);
            eprintln!("scroll test: {kind}");
            // Put the remote pointer over the target document; synthetic
            // local mouse moves do not reach winit in this environment.
            self.cursor = (600, 400);
            // Move only: a click could select a Finder item or follow a link.
            // SS_SCROLL_AT=x,y overrides the target point for measurements.
            let at = std::env::var("SS_SCROLL_AT")
                .ok()
                .and_then(|v| {
                    let mut it = v.split(',');
                    Some((
                        it.next()?.trim().parse::<u16>().ok()?,
                        it.next()?.trim().parse::<u16>().ok()?,
                    ))
                })
                .unwrap_or((600, 400));
            self.cursor = at;
            self.send(pointer_event(0, at.0, at.1));
            thread::sleep(Duration::from_millis(150));
            let step = |dy: f32, phase: Phase| Gesture::Scroll {
                dx: 0.0,
                dy,
                phase,
                precise: true,
                momentum: None,
            };
            // points per event, events, and whether a touch stops the glide.
            let (per_event, events, stop_after) = match kind.as_str() {
                "drag" => (3.0, 20, false),
                "medium" => (10.0, 20, false),
                "stop" => (40.0, 10, true),
                "hardflick" => (80.0, 10, false),
                "stress" => (40.0, 10, false),
                "precise100" => (5.0, 20, false),
                "precise5" => (5.0, 1, false),
                "precise1" => (1.0, 20, false),
                "jitter" => (10.0, 20, false),
                "diagonal" => (10.0, 20, false),
                "turn" => (10.0, 38, false),
                _ => (40.0, 10, false),
            };
            self.queue_scroll(step(0.0, Phase::Began));
            for i in 0..events {
                let (dx, dy) = match kind.as_str() {
                    // Mostly vertical with deliberate sideways jitter.
                    "jitter" => (if i % 2 == 0 { 3.0 } else { -3.0 }, -per_event),
                    // Equal both ways.
                    "diagonal" => (-per_event, -per_event),
                    // Vertical for the first half, then a deliberate turn.
                    "turn" => {
                        if i < 19 {
                            (0.0, -per_event)
                        } else {
                            (-per_event, 0.0)
                        }
                    }
                    _ => (0.0, -per_event),
                };
                self.queue_scroll(Gesture::Scroll {
                    dx,
                    dy,
                    phase: Phase::Changed,
                    precise: true,
                    momentum: None,
                });
                thread::sleep(Duration::from_millis(16));
            }
            self.queue_scroll(step(0.0, Phase::Ended));
            if kind == "stress" {
                // Repeat the flick so a sustained scroll can be measured.
                self.scroll_stress = 8;
                self.scroll_stress_next = Some(Instant::now() + Duration::from_millis(700));
            }
            if stop_after {
                // Do not block the event loop: the glide is ticked from
                // about_to_wait, so the touch is scheduled instead of slept.
                self.scroll_stop_at = Some(Instant::now() + Duration::from_millis(250));
            }
        }
        if let Some(at) = self.scroll_stop_at {
            if Instant::now() >= at {
                self.scroll_stop_at = None;
                eprintln!("scroll test: touch to stop the glide");
                self.queue_scroll(Gesture::Scroll {
                    dx: 0.0,
                    dy: 0.0,
                    phase: Phase::Began,
                    precise: true,
                    momentum: None,
                });
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(at));
            }
        }
        if let Some(at) = self.scroll_stress_next {
            if Instant::now() >= at {
                if self.scroll_stress > 0 {
                    self.scroll_stress -= 1;
                    self.scroll_stress_next = Some(Instant::now() + Duration::from_millis(700));
                    self.queue_scroll(Gesture::Scroll {
                        dx: 0.0,
                        dy: 0.0,
                        phase: Phase::Began,
                        precise: true,
                        momentum: None,
                    });
                    for _ in 0..10 {
                        self.queue_scroll(Gesture::Scroll {
                            dx: 0.0,
                            dy: -40.0,
                            phase: Phase::Changed,
                            precise: true,
                            momentum: None,
                        });
                        thread::sleep(Duration::from_millis(16));
                    }
                    self.queue_scroll(Gesture::Scroll {
                        dx: 0.0,
                        dy: 0.0,
                        phase: Phase::Ended,
                        precise: true,
                        momentum: None,
                    });
                } else {
                    self.scroll_stress_next = None;
                }
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(at));
            }
        }
        if let Some(at) = self.scroll_stop_at {
            if Instant::now() >= at {
                self.scroll_stop_at = None;
                eprintln!("scroll test: touch to stop the glide");
                self.queue_scroll(Gesture::Scroll {
                    dx: 0.0,
                    dy: 0.0,
                    phase: Phase::Began,
                    precise: true,
                    momentum: None,
                });
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(at));
            }
        }
        if let Some(at) = self.scroll_stress_next {
            if Instant::now() >= at {
                if self.scroll_stress > 0 {
                    self.scroll_stress -= 1;
                    self.scroll_stress_next = Some(Instant::now() + Duration::from_millis(700));
                    self.queue_scroll(Gesture::Scroll {
                        dx: 0.0,
                        dy: 0.0,
                        phase: Phase::Began,
                        precise: true,
                        momentum: None,
                    });
                    for _ in 0..10 {
                        self.queue_scroll(Gesture::Scroll {
                            dx: 0.0,
                            dy: -40.0,
                            phase: Phase::Changed,
                            precise: true,
                            momentum: None,
                        });
                        thread::sleep(Duration::from_millis(16));
                    }
                    self.queue_scroll(Gesture::Scroll {
                        dx: 0.0,
                        dy: 0.0,
                        phase: Phase::Ended,
                        precise: true,
                        momentum: None,
                    });
                } else {
                    self.scroll_stress_next = None;
                }
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(at));
            }
        }
        // A coalesced scroll batch must not sit until the next wheel event:
        // flush it when the interval expires, or wake up to do so. Without
        // this the tail of a flick (and every wheel tick) would arrive late.
        if self.scroll_pending.is_some() {
            let deadline = self.scroll_last + SCROLL_INTERVAL;
            if Instant::now() >= deadline {
                self.flush_scroll();
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
            }
        } else if !self.demo_cursor {
            // While the keyboard is captured, keep waking so the tap watchdog
            // can tell "idle" from "wedged". An idle app that never ticks is
            // indistinguishable from a hung one, and the watchdog would then
            // release a perfectly healthy grab.
            if grab::is_active() {
                event_loop.set_control_flow(ControlFlow::WaitUntil(
                    Instant::now() + Duration::from_millis(400),
                ));
            } else {
                event_loop.set_control_flow(ControlFlow::Wait);
            }
        }
        if self.rotate_active
            && self.rotate_last.map_or(true, |t| t.elapsed() > Duration::from_millis(150))
        {
            self.send(gesture_rotate(0.0, 4, 31, self.pinch_x, self.pinch_y));
            self.rotate_active = false;
        }
        // Metadata (display, cursor, clipboard, file copy) from the reader.
        while let Ok(update) = self.rx.try_recv() {
            self.apply_update(update);
        }
        // The newest decoded frame, if the reader produced one since the last
        // paint. Older frames were already replaced in the slot, never queued.
        let frame = self
            .frame_slot
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        if let Some((w, h, rects, full)) = frame {
            if (w, h) != (self.fb_w, self.fb_h) {
                self.fb_w = w;
                self.fb_h = h;
                self.fb = vec![0u32; w as usize * h as usize];
                // Force the draw mapping to be rebuilt for the new geometry.
                self.render_size = (0, 0);
            }
            let dst_w = w as usize;
            if self.debug_input {
                for (x, y, rw, rh, _) in &rects {
                    eprintln!("ui frame {}x{} rect {}x{}+{}+{}", w, h, rw, rh, x, y);
                }
            }
            for (x, y, rw, rh, pixels) in rects {
                let (x, y, rw, rh) = (x as usize, y as usize, rw as usize, rh as usize);
                if x + rw > dst_w || y + rh > h as usize {
                    continue;
                }
                for row in 0..rh {
                    let dst = (y + row) * dst_w + x;
                    let src = row * rw;
                    self.fb[dst..dst + rw].copy_from_slice(&pixels[src..src + rw]);
                }
            }
            self.frames += 1;
            self.full_frame |= full;
            LAST_APPLIED_MS.store(now_ms(), Ordering::Relaxed);
            PUSH_PAINTED.store(true, Ordering::Relaxed);
            self.record_input_latency();
            if self.frames >= self.lat_next_report {
                self.report_input_latency("latency report");
                self.lat_next_report = self.frames + 50;
            }
            self.dirty = true;
            if let Some(win) = &self.window {
                win.request_redraw();
            }
        }
        if let Some(deadline) = self.deadline {
            if Instant::now() >= deadline {
                if let Some(path) = self.dump_path.take() {
                    let frame = self.dump_frame();
                    match write_png(&path, self.fb_w, self.fb_h, &frame) {
                        Ok(()) => eprintln!("wrote {path} at deadline"),
                        Err(e) => eprintln!("dump failed: {e:#}"),
                    }
                }
                event_loop.exit();
                return;
            }
        }
        if self.full_frame && self.frames >= self.dump_after {
            if let Some(path) = self.dump_path.take() {
                let frame = self.dump_frame();
                match write_png(&path, self.fb_w, self.fb_h, &frame) {
                    Ok(()) => eprintln!("wrote {path} after {} frames", self.frames),
                    Err(e) => eprintln!("dump failed: {e:#}"),
                }
                if self.exit_after_dump {
                    event_loop.exit();
                }
            }
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.report_input_latency("exit summary");
        // Never leave a modifier down on the remote when we go away, and drop
        // the local keyboard grab before the window is destroyed.
        if let Some(grab) = &self.grab {
            grab.set_enabled(false);
            eprintln!("grab: disabled (shutdown)");
        }
        self.modifiers = ModifiersState::empty();
        self.sync_modifiers(ModifiersState::empty());
        self.surface = None;
        self.window = None;
    }
}

fn main() -> Result<()> {
    let mut host = "mm.local".to_string();
    let mut port = 5900u16;
    let mut user = "tai".to_string();
    let mut password = std::env::var("SS_PASSWORD").unwrap_or_default();
    let mut evdev = false;
    let mut dump_path: Option<String> = None;
    let mut dump_after = 4u32;
    let mut exit_after_dump = false;
    let mut scale = 1.0f64;
    let mut debug_input = false;
    // Clipboard sync is automatic and bidirectional. The 0x0b fetch is sent
    // as the full 8-byte message (a 1-byte fetch shifts the server's message
    // parser by 7 bytes and closes the session) and asks for full data.
    let mut clipboard = true;
    // Kept for compatibility with existing lab scripts; the write path is part
    // of the default duplex now.
    let mut clipboard_send = false;
    // Experimental: server-pushed dirty rectangles instead of polling.
    let mut auto_updates = false;
    // Framebuffer requests kept in flight so a frame is already encoding while
    // we render the previous one (no request/response stall per frame).
    let mut pipeline: usize = 3;
    // Banded refresh: only *non-incremental* sub-region requests are answered
    // reliably (incremental ones stall when a band has no changes). Sweeping
    // bands during input turns one 700-880 ms full repaint into ~80 ms steps.
    let mut bands: usize = 8;
    // Encoding experiment: 0 = raw, 16 = ZRLE, 6 = Zlib (default list).
    let mut encoding: Option<i32> = None;
    let mut ime = false;
    // MultiVariant (0x3f3): GPU-encoded tiles, ~40 ms per full frame instead
    // of the ~640 ms Zlib round trip.
    let mut mvs = false;
    // Server push is the default for MVS; `--poll` forces the request loop.
    // `--push` is kept as an accepted no-op alias for the default.
    let mut poll = false;
    // Lab helper: scripted remote pointer tour that dumps a PNG per stop, so
    // cursor-shape rendering can be verified without a human at the mouse.
    let mut demo_cursor = false;
    // File-copy triggers until the drag UX is the only path: send local files
    // through the verified 0x22 chain and request remote files into
    // ~/Downloads. Repeatable; no protocol is guessed here.
    let mut send_files: Vec<PathBuf> = Vec::new();
    let mut recv_files: Vec<String> = Vec::new();
    // `--pull <path>`: pull an absolute remote path without touching the
    // remote clipboard. Works even when the clipboard carries no path at all.
    let mut pull_paths: Vec<String> = Vec::new();
    // The remote's layout table (`us`, `jis`); the local layout is whatever
    // the OS composes, so it never needs naming.
    let mut coast = false;
    let mut keymap_name: Option<String> = None;
    // Lab hook: scripted gesture for measuring the scroll model
    // (`flick`, `drag`, `medium`, `stop`, `jitter`, `diagonal`, `turn`).
    let mut scroll_test = String::new();
    // macOS keyboard grab (`grab.rs`).
    let mut no_grab = false;
    let mut release_chord: Option<String> = None;
    let mut exit_after_secs: Option<f64> = None;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "--host" if i + 1 < args.len() => {
                host = args[i + 1].clone();
                i += 2;
            }
            "--port" if i + 1 < args.len() => {
                port = args[i + 1].parse()?;
                i += 2;
            }
            "--user" if i + 1 < args.len() => {
                user = args[i + 1].clone();
                i += 2;
            }
            "--password" if i + 1 < args.len() => {
                password = args[i + 1].clone();
                i += 2;
            }
            "--evdev" => {
                evdev = true;
                i += 1;
            }
            "--dump" if i + 1 < args.len() => {
                dump_path = Some(args[i + 1].clone());
                i += 2;
            }
            "--dump-after" if i + 1 < args.len() => {
                dump_after = args[i + 1].parse()?;
                i += 2;
            }
            "--exit-after-dump" => {
                exit_after_dump = true;
                i += 1;
            }
            "--scale" if i + 1 < args.len() => {
                scale = args[i + 1].parse()?;
                i += 2;
            }
            "--debug-input" => {
                debug_input = true;
                i += 1;
            }
            "--no-clipboard" => {
                clipboard = false;
                i += 1;
            }
            "--auto" => {
                auto_updates = true;
                i += 1;
            }
            "--encoding" if i + 1 < args.len() => {
                encoding = Some(args[i + 1].parse::<i32>()?);
                i += 2;
            }
            "--bands" if i + 1 < args.len() => {
                bands = args[i + 1].parse::<usize>()?.clamp(1, 64);
                i += 2;
            }
            "--pipeline" if i + 1 < args.len() => {
                pipeline = args[i + 1].parse::<usize>()?.clamp(1, 8);
                i += 2;
            }
            "--clipboard" => {
                clipboard = true;
                i += 1;
            }
            "--send-clipboard" => {
                clipboard_send = true;
                clipboard = true;
                i += 1;
            }
            "--ime" => {
                ime = true;
                i += 1;
            }
            "--scroll-test" if i + 1 < args.len() => {
                scroll_test = args[i + 1].clone();
                i += 2;
            }
            "--no-grab" => {
                no_grab = true;
                i += 1;
            }
            "--release-grab" if i + 1 < args.len() => {
                release_chord = Some(args[i + 1].clone());
                i += 2;
            }
            "--scroll-speed" if i + 1 < args.len() => {
                let speed: f32 = args[i + 1].parse()?;
                if !(0.1..=20.0).contains(&speed) {
                    eprintln!("--scroll-speed must be between 0.1 and 20.0");
                    std::process::exit(2);
                }
                SCROLL_SPEED.store(speed.to_bits(), std::sync::atomic::Ordering::Relaxed);
                i += 2;
            }
            "--coast" => {
                coast = true;
                i += 1;
            }
            "--no-coast" => {
                // Kept as an accepted no-op: dropping momentum is the default.
                coast = false;
                i += 1;
            }
            "--keymap" if i + 1 < args.len() => {
                keymap_name = Some(args[i + 1].to_ascii_lowercase());
                i += 2;
            }
            "--pull" if i + 1 < args.len() => {
                pull_paths.push(args[i + 1].clone());
                i += 2;
            }
            "--send-file" if i + 1 < args.len() => {
                send_files.push(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--recv-file" if i + 1 < args.len() => {
                recv_files.push(args[i + 1].clone());
                i += 2;
            }
            "--mvs" => {
                mvs = true;
                i += 1;
            }
            "--push" => {
                // Accepted for compatibility: push is the default for --mvs.
                i += 1;
            }
            "--demo-cursor" => {
                demo_cursor = true;
                i += 1;
            }
            "--poll" => {
                poll = true;
                i += 1;
            }
            "--exit-after-secs" if i + 1 < args.len() => {
                exit_after_secs = Some(args[i + 1].parse()?);
                i += 2;
            }
            other => {
                eprintln!("unknown argument {other}");
                std::process::exit(2);
            }
        }
    }

    // Apple's client is push-based, so `--mvs` arms server push by default.
    // `--poll` is the escape hatch and the A/B switch; the Zlib path never
    // arms 0x09 either way.
    let push = mvs && !poll;
    if mvs {
        if push {
            eprintln!(
                "mvs: server push (arm 0x09 screen 0 + one 0x03); use --poll for the request loop"
            );
        } else {
            eprintln!("mvs: request loop (--poll); one incremental request per applied update");
        }
    }

    // Banding existed to hide the 700-880 ms deflate full repaint. An MVS full
    // frame is 44-49 ms, so in the request loop the sequential band sweep is
    // pure added latency (server push never requests at all).
    let bands = if mvs && !push && bands > 1 {
        eprintln!("mvs: banded refresh disabled");
        1
    } else {
        bands
    };

    let keymap = match keymap_name.as_deref() {
        None => &US,
        Some(name) => match Keymap::by_name(name) {
            Some(map) => map,
            None => {
                eprintln!("unknown --keymap {name:?}; expected us or jis");
                std::process::exit(2);
            }
        },
    };
    eprintln!("keymap: {} (remote layout)", keymap.name);
    #[cfg(target_os = "macos")]
    {
        if momentum::start() {
            eprintln!("scroll: momentum-phase monitor installed");
        } else {
            eprintln!("scroll: momentum-phase monitor unavailable; forwarding every scroll event");
        }
    }
    if let Ok(v) = std::env::var("SS_SCROLL_SUBTYPE") {
        match v.trim().parse::<u8>() {
            Ok(8) => {
                SCROLL_SUBTYPE.store(8, std::sync::atomic::Ordering::Relaxed);
                eprintln!("scroll: EXPERIMENTAL subtype 8 (gesture scroll, pixel deltas)");
            }
            Ok(11) => {}
            _ => eprintln!("scroll: SS_SCROLL_SUBTYPE must be 8 or 11; using 11"),
        }
    }

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    #[cfg(target_os = "macos")]
    let grab = if no_grab {
        eprintln!("grab: disabled (--no-grab); local shortcuts stay local");
        None
    } else {
        grab::request_permission();
        let chord = match release_chord.as_deref() {
            None => grab::Chord::default_hold(),
            Some(spec) => match grab::parse_chord(spec) {
                Ok(chord) => chord,
                Err(error) => {
                    eprintln!("--release-grab {spec:?}: {error}; using the default");
                    grab::Chord::default_hold()
                }
            },
        };
        eprintln!(
            "grab: keyboard captured while focused; {} to release",
            chord.describe()
        );
        let proxy = std::sync::Mutex::new(proxy.clone());
        match grab::start(
            move |event| {
                if let Ok(proxy) = proxy.lock() {
                    let _ = proxy.send_event(UserEvent::Grab(event));
                }
            },
            chord,
        ) {
            Ok(handle) => Some(handle),
            Err(reason) => {
                eprintln!("grab: cannot create the keyboard event tap: {reason}");
                let exe = std::env::current_exe()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "ss-gui".to_string());
                eprintln!(
                    "grab: grant Accessibility to this binary: System Settings > Privacy & Security > Accessibility -> + -> {exe}"
                );
                eprintln!(
                    "grab: also add it under Privacy & Security > Input Monitoring if it is listed there"
                );
                eprintln!(
                    "grab: continuing without the grab; keyboard shortcuts stay local"
                );
                None
            }
        }
    };
    #[cfg(not(target_os = "macos"))]
    let grab: Option<GrabHandle> = {
        if !no_grab {
            eprintln!("grab: not available on this platform");
        }
        None
    };


    eprintln!("connecting to {host}:{port} as {user}...");
    let mut session = AppleSession::connect(&host, port, &user, &password)?;
    eprintln!(
        "connected: {}x{} — click the window to focus. Use --debug-input to trace events.",
        session.info.width, session.info.height
    );
    session.start_streaming()?;
    if (scale - 1.0).abs() > 0.001 {
        eprintln!("warning: --scale {scale} is experimental; this server build may stop incremental updates");
        session.set_scale(scale)?;
    }
    // Only advertise encodings `read_update` can consume end to end: an
    // unknown rect kills the session. Apple 0x450 is the alpha cursor
    // (w*h*5 inflated: BGRX plane + alpha plane) and 0x44c reports pointer
    // warps; the server prefers 0x450 over RichCursor -239, so -239 stays out
    // of the list. Cursor rects only arrive while push (0x09) is armed.
    if let Some(enc) = encoding {
        session.send_message(&rfb_proto::set_encodings(&[enc, 0x450, 0x44c]))?;
        eprintln!("advertised encoding {enc} + cursor 0x450/0x44c");
    } else if mvs {
        session.send_message(&rfb_proto::set_encodings(&[0x3f3, 0x450, 0x44c]))?;
        eprintln!("advertised MVS (0x3f3) + cursor 0x450/0x44c");
    } else {
        session.send_message(&rfb_proto::set_encodings(&[6, 0x450, 0x44c, 0x451, 0x453]))?;
        eprintln!("advertised Zlib (6) + cursor 0x450/0x44c + DisplayInfo");
    }
    if clipboard {
        session.send_message(&rfb_proto::auto_pasteboard(1))?;
        eprintln!("clipboard: automatic duplex (AutoPasteboard start, full-data fetches)");
        let _ = clipboard_send;
    }
    let req_w = session.info.width;
    let req_h = session.info.height;
    let fb_w = ((req_w as f64 * scale).round() as u16).max(1);
    let fb_h = ((req_h as f64 * scale).round() as u16).max(1);
    let input_w = req_w;
    let input_h = req_h;
    // Arm the framebuffer sender on the pre-split connection. Sending this
    // through the writer thread after split leaves the GUI without
    // server-pushed updates, while the same messages work from the CLI.
    if push {
        // codecs.md 2.20.4: server push is enabled by 0x09 with
        // `selected_screen = 0` (0xffffffff does not free-run on this build),
        // followed by exactly ONE non-incremental 0x03. No polling after this.
        session.send_message(&rfb_proto::auto_framebuffer_update_custom(
            1, 0, fb_w, fb_h,
        ))?;
        session.send_message(&framebuffer_update_request(fb_w, fb_h, false))?;
        eprintln!(
            "push armed: 0x09 (version 1, screen 0) {fb_w}x{fb_h} + one non-incremental 0x03; no polling"
        );
    } else {
        if auto_updates {
            session.send_message(&rfb_proto::auto_framebuffer_update(fb_w, fb_h))?;
        }
        session.send_message(&framebuffer_update_request(fb_w, fb_h, false))?;
        for _ in 1..pipeline {
            session.send_message(&framebuffer_update_request(fb_w, fb_h, true))?;
        }
    }
    let (reader, writer) = session.split()?;

    let (tx_out, rx_out) = mpsc::channel::<Vec<u8>>();
    // Bounded: if the GUI cannot keep up, apply backpressure instead of
    // queueing stale frames (which makes interaction feel sluggish).
    let (tx_upd, rx_upd) = mpsc::channel::<Update>();
    // One-slot frame handoff: the reader decodes, the UI presents. A newer
    // frame always replaces an older one, so a slow UI skips ahead instead of
    // replaying a backlog.
    let frame_slot: Arc<std::sync::Mutex<Option<Frame>>> =
        Arc::new(std::sync::Mutex::new(None));

    // File copy: received items always land in ~/Downloads; dropped files are
    // sent to /Users/<remote user>/Downloads (the native client's shape).
    let file_dest = filecopy::remote_destination(&user);
    let recv_dir = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Downloads");
    eprintln!(
        "filecopy: receive directory {}; remote destination {file_dest}",
        recv_dir.display()
    );
    let (tx_fc, rx_fc) = mpsc::channel();
    let pull_pending: Arc<std::sync::Mutex<Option<PullState>>> =
        Arc::new(std::sync::Mutex::new(None));
    // Last absolute path the remote clipboard carried (from whichever text
    // flavour held it). A fresh archive always decides the pull itself; this
    // is only a fallback for a fetch that gets no archive at all, and it is
    // cleared as soon as the remote clipboard holds non-path content.
    let remote_path: Arc<std::sync::Mutex<Option<String>>> =
        Arc::new(std::sync::Mutex::new(None));
    eprintln!("pull-from-remote: Cmd+Shift+V (or Ctrl+Shift+V) armed");
    {
        let recv_dir = recv_dir.clone();
        let verbose = debug_input;
        thread::spawn(move || filecopy::receive_worker(rx_fc, recv_dir, verbose));
    }
    for path in send_files.drain(..) {
        let tx = tx_out.clone();
        let destination = file_dest.clone();
        let verbose = debug_input;
        thread::spawn(move || {
            if let Err(error) =
                filecopy::send_file(tx, path.clone(), destination, verbose)
            {
                eprintln!("filecopy: send {} failed: {error:#}", path.display());
            }
        });
    }
    for path in recv_files.drain(..) {
        let session = filecopy::next_session();
        eprintln!("filecopy: requesting remote file {path} (session {session})");
        let _ = tx_out.send(rfb_proto::filecopy::start_file_send(&path, session));
    }
    for path in pull_paths.drain(..) {
        if !filecopy::is_absolute_path(&path) {
            eprintln!(
                "pull-from-remote: --pull {path:?} ignored: not an absolute path"
            );
            continue;
        }
        let session = filecopy::next_session();
        eprintln!(
            "pull-from-remote: --pull {path} -> {} (session {session})",
            recv_dir.display()
        );
        let _ = tx_out.send(rfb_proto::filecopy::start_file_send(&path, session));
    }

    if evdev {
        #[cfg(target_os = "linux")]
        {
            let tx = tx_out.clone();
            match gesture_input::linux::spawn_default(move |g| {
                for body in gesture_bodies(g, input_w, input_h) {
                    let _ = tx.send(body);
                }
            }) {
                Ok(names) => eprintln!("evdev gesture devices: {names:?}"),
                Err(e) => eprintln!("evdev: {e}"),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            eprintln!("--evdev is Linux-only; use the trackpad/winit gestures on this platform");
        }
    }

    if push {
        // Narrow watchdog: once the first pushed frame has been painted, if the
        // stream goes quiet for more than 2 s, say so once and stop waiting.
        // Deliberately does NOT fall back to polling, so a broken push stream
        // is visible instead of silently costing a request per keystroke.
        thread::spawn(move || loop {
            thread::sleep(Duration::from_millis(200));
            if !PUSH_PAINTED.load(Ordering::Relaxed) {
                continue;
            }
            let quiet = now_ms().saturating_sub(LAST_APPLIED_MS.load(Ordering::Relaxed));
            if quiet > 2000 {
                eprintln!(
                    "[{:6} ms] push: nothing applied for {quiet} ms after the initial paint; giving up on push — re-run with --poll to use the request loop",
                    now_ms()
                );
                break;
            }
        });
    }

    let debug_writer = debug_input;
    thread::spawn(move || {
        let mut writer = writer;
        while let Ok(body) = rx_out.recv() {
            if debug_writer && body.first() != Some(&0) {
                let shown = body.len().min(20);
                eprintln!("send type=0x{:02x} len={} {:02x?}", body[0], body.len(), &body[..shown]);
            }
            if let Err(e) = writer.send(&body) {
                eprintln!("write error: {e:#}");
                break;
            }
        }
    });

    // Text most recently applied from the remote, so we do not echo it back.
    let clip_echo: Arc<std::sync::Mutex<Option<String>>> =
        Arc::new(std::sync::Mutex::new(None));
    // Fingerprint of the last image applied from the remote, so the local
    // watcher does not bounce it back.
    let image_echo: Arc<std::sync::Mutex<Option<u64>>> =
        Arc::new(std::sync::Mutex::new(None));
    if clipboard {
        let tx_clip = tx_out.clone();
        let debug_clip = debug_input;
        let clip_echo = clip_echo.clone();
        let image_echo = image_echo.clone();
        thread::spawn(move || {
            let mut last_text = String::new();
            loop {
                thread::sleep(Duration::from_millis(700));
                let text = local_clipboard_text().unwrap_or_default();
                if !text.is_empty() {
                    if text == last_text {
                        continue;
                    }
                    let echo = clip_echo
                        .lock()
                        .map(|e| e.as_deref() == Some(text.as_str()))
                        .unwrap_or(false);
                    last_text = text.clone();
                    if echo {
                        // We just wrote this into the local clipboard; do not
                        // send it straight back to the server.
                        continue;
                    }
                    if debug_clip {
                        eprintln!("clipboard -> {:?}", text);
                    }
                    // Remember what we sent: the server echoes the viewer's
                    // own pasteboard back via a cmd=2 fetch.
                    if let Ok(mut e) = clip_echo.lock() {
                        *e = Some(text.clone());
                    }
                    if tx_clip.send(client_cut_text(text.as_bytes())).is_err() {
                        break;
                    }
                    continue;
                }
                // No text: an image on the local clipboard is pushed as a
                // viewer→server 0x1f pasteboard archive (0x06 is text-only).
                let Some((uti, data)) = local_clipboard_image() else { continue };
                let h = hash_bytes(&data);
                if image_echo.lock().map(|e| *e == Some(h)).unwrap_or(false) {
                    continue;
                }
                if let Ok(mut e) = image_echo.lock() {
                    *e = Some(h);
                }
                let archive = pasteboard_archive(&[image_item(uti, &data)]);
                if debug_clip {
                    eprintln!("clipboard -> image {uti} {} bytes", data.len());
                }
                if tx_clip
                    .send(viewer_clipboard_send(&archive, false))
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    let clip_echo_reader = clip_echo.clone();
    let image_echo_reader = image_echo.clone();
    let tx_reader = tx_out.clone();
    let tx_fc_reader = tx_fc.clone();
    let pull_pending_reader = pull_pending.clone();
    let remote_path_reader = remote_path.clone();
    let recv_dir_reader = recv_dir.clone();
    let reader_proxy = proxy.clone();
    let frame_slot_reader = frame_slot.clone();
    let debug_reader = debug_input;
    thread::spawn(move || {
        let mut reader = reader;
        let mut decoder = Decoder::new(fb_w, fb_h, mvs, debug_reader);
        if debug_reader {
            thread::spawn(|| loop {
                thread::sleep(Duration::from_secs(2));
                eprintln!(
                    "reader alive: updates={} last_read_ms_ago={}",
                    READER_UPDATES.load(Ordering::Relaxed),
                    {
                        let start = *START.get_or_init(Instant::now);
                        start.elapsed().as_millis() as u64
                            - READER_ALIVE_MS.load(Ordering::Relaxed)
                    }
                );
            });
        }
        let band_h = ((fb_h as usize + bands - 1) / bands).max(1);
        let band_rect = |i: usize| -> (u16, u16, u16, u16) {
            let y = i * band_h;
            let h = (fb_h as usize - y).min(band_h) as u16;
            (0, y as u16, fb_w, h)
        };
        let mut next_band = 0usize;
        let clip_wanted = clipboard;
        let mut full_seen = !mvs;
        grab::beat_reader();
        if clip_wanted {
            let _ = tx_reader.send(clipboard_fetch());
        }
        loop {
            let read_started = Instant::now();
            let upd = reader.read_update();
            let read_ms = read_started.elapsed().as_secs_f64() * 1000.0;
            // The reader is a second liveness witness: the tap watchdog only
            // fires when neither the event loop nor the reader has ticked.
            grab::beat_reader();
            match upd {
                Ok(update) => {
                    for event in update.filecopy.iter() {
                        let _ = tx_fc_reader.send(event.clone());
                    }
                    // Cmd+Shift+V: a path resolves the pull immediately;
                    // otherwise remember what the archive actually held (the
                    // retry thread quotes it if no path arrives in time).
                    let mut consumed_by_pull = false;
                    let mut pulled: Option<filecopy::PullSource> = None;
                    if let Ok(mut pending) = pull_pending_reader.lock() {
                        if let Some(state) = pending.as_mut() {
                            if !update.clipboard.is_empty() {
                                match filecopy::pull_from_sends(&update.clipboard) {
                                    Ok(filecopy::PullOutcome::File(source)) => {
                                        pulled = Some(source);
                                    }
                                    Ok(_) => {
                                        state.last_seen = Some(
                                            filecopy::describe_sends(&update.clipboard),
                                        );
                                    }
                                    Err(error) => {
                                        state.last_seen =
                                            Some(format!("archive decode failed: {error:#}"));
                                    }
                                }
                            }
                        }
                        if pulled.is_some() {
                            *pending = None;
                        }
                    }
                    if let Some(source) = pulled {
                        consumed_by_pull = true;
                        let session = filecopy::next_session();
                        eprintln!(
                            "pull-from-remote: stage=fresh-archive {} -> {} (session {session})",
                            source.path,
                            recv_dir_reader.display()
                        );
                        let name = source.path.rsplit('/').next().unwrap_or("file");
                        let _ = reader_proxy.send_event(UserEvent::PullStatus(format!(
                            "ss-clone — pulling {name}…"
                        )));
                        // Keep the cached remote path in step with what was
                        // just pulled.
                        if let Ok(mut p) = remote_path_reader.lock() {
                            *p = Some(source.path.clone());
                        }
                        let _ = tx_reader.send(rfb_proto::filecopy::start_file_send(
                            &source.path,
                            session,
                        ));
                    }
                    if clip_wanted && !consumed_by_pull {
                        if update.clipboard_changed {
                            let _ = tx_reader.send(clipboard_fetch());
                        }
                        for send in &update.clipboard {
                            let archive = match send.archive() {
                                Ok(a) => a,
                                Err(e) => {
                                    eprintln!("clipboard archive: {e:#}");
                                    continue;
                                }
                            };
                            // Prefer an image flavour; fall back to text.
                            match pasteboard_image(&archive) {
                                Ok(Some(img)) => {
                                    if debug_reader {
                                        eprintln!(
                                            "clipboard <- image {} {} bytes",
                                            img.uti,
                                            img.data.len()
                                        );
                                    }
                                    if let Ok(mut e) = image_echo_reader.lock() {
                                        *e = Some(hash_bytes(&img.data));
                                    }
                                    if let Ok(mut p) = remote_path_reader.lock() {
                                        *p = None;
                                    }
                                    set_local_clipboard_image(&img.uti, &img.data);
                                    continue;
                                }
                                Ok(None) => {}
                                Err(e) => eprintln!("clipboard image decode: {e:#}"),
                            }
                            match filecopy::local_clipboard(&archive) {
                                Ok(Some(choice)) => {
                                    // The winning flavour is logged: Finder's
                                    // Option+Cmd+C sends three text items, all
                                    // the absolute path; a plain copy sends the
                                    // bare name, which must not be turned into
                                    // a pull.
                                    let echo = clip_echo_reader
                                        .lock()
                                        .map(|e| e.as_deref() == Some(choice.text.as_str()))
                                        .unwrap_or(false);
                                    if debug_reader {
                                        eprintln!(
                                            "clipboard <- {:?} (source={}){}",
                                            choice.text,
                                            choice.source,
                                            if echo { " (echo, skipped)" } else { "" }
                                        );
                                    }
                                    if !echo {
                                        if let Ok(mut e) = clip_echo_reader.lock() {
                                            *e = Some(choice.text.clone());
                                        }
                                        if let Ok(mut p) = remote_path_reader.lock() {
                                            *p = filecopy::is_absolute_path(&choice.text)
                                                .then(|| choice.text.clone());
                                        }
                                        set_local_clipboard(&choice.text);
                                    }
                                }
                                Ok(None) => {}
                                Err(e) => eprintln!("clipboard decode: {e:#}"),
                            }
                        }
                    }
                    if debug_reader {
                        let ms = START
                            .get_or_init(Instant::now)
                            .elapsed()
                            .as_millis() as u64;
                        eprintln!("[{ms:6} ms] read took {read_ms:.1} ms");
                        let desc: Vec<_> = update
                            .rects
                            .iter()
                            .map(|r| (r.x, r.y, r.w, r.h, r.encoding, r.pixels.len()))
                            .collect();
                        eprintln!("[{ms:6} ms] update rects={desc:?}");
                    }
                    READER_UPDATES.fetch_add(1, Ordering::Relaxed);
                    READER_ALIVE_MS.store(
                        START.get_or_init(Instant::now).elapsed().as_millis() as u64,
                        Ordering::Relaxed,
                    );
                    let layout = update.rects.iter().any(|r| r.encoding == 0x451);
                    let full_mvs = update
                        .rects
                        .iter()
                        .any(|r| r.encoding == 0x3f3 && r.w == fb_w && r.h == fb_h);
                    // Decode here, on the reader thread: an MVS rect can be
                    // 12,000 DCT blocks, and doing that on the event loop
                    // starved the window. The UI gets a ready frame.
                    for r in &update.rects {
                        decoder.apply(r);
                    }
                    publish_frame(&mut decoder, &frame_slot_reader);
                    // The UI has no use for the encoded rects any more.
                    let mut meta = update;
                    meta.rects.clear();
                    if tx_upd.send(meta).is_err() {
                        break;
                    }
                    let _ = reader_proxy.send_event(UserEvent::Refresh);
                    if push {
                        // Server-push mode: never request. A display layout
                        // change drains the pushed stream, so re-arm 0x09 plus
                        // one non-incremental 0x03 exactly like the native
                        // client; nothing else.
                        if layout {
                            let _ = tx_reader.send(
                                rfb_proto::auto_framebuffer_update_custom(1, 0, fb_w, fb_h),
                            );
                            let _ = tx_reader
                                .send(framebuffer_update_request(fb_w, fb_h, false));
                            if debug_reader {
                                eprintln!(
                                    "[{:6} ms] display layout: re-armed 0x09 (screen 0) + one non-incremental 0x03",
                                    now_ms()
                                );
                            }
                        }
                        continue;
                    }
                    // This server build does not reliably free-run after the
                    // first auto frame. Poll with the *scaled* geometry so
                    // post-input dirty rectangles are always delivered.
                    if !full_seen {
                        // SetEncodings is answered with metadata and the type-2
                        // quant body first; the first full tile body follows an
                        // incremental request. Nudge until it has landed.
                        if full_mvs {
                            full_seen = true;
                        }
                        let _ = tx_reader.send(framebuffer_update_request(fb_w, fb_h, true));
                        continue;
                    }
                    if auto_updates {
                        // Server-push mode. The stream drains on a display
                        // layout change; re-arm 0x09 *and* a non-incremental
                        // 0x03 exactly like the native client.
                        if layout {
                            let _ = tx_reader
                                .send(rfb_proto::auto_framebuffer_update(fb_w, fb_h));
                            let _ = tx_reader.send(framebuffer_update_request(
                                fb_w, fb_h, false,
                            ));
                        }
                        continue;
                    }
                    if mvs {
                        // MVS: no banding, no sleep. Re-issue a full-frame
                        // incremental request the moment the previous update is
                        // fanned out, so `pipeline` requests stay outstanding
                        // and a keystroke is answered by one already queued on
                        // the server instead of waiting for a fresh request.
                        let _ = tx_reader.send(framebuffer_update_request(fb_w, fb_h, true));
                    } else if bands > 1 && input_is_hot() {
                        // Progressive refresh: re-request the next band in full.
                        let focus = FOCUS_Y.swap(u64::MAX, Ordering::Relaxed);
                        if focus != u64::MAX {
                            next_band = (focus as usize / band_h).min(bands - 1);
                        }
                        let (bx, by, bw, bh) = band_rect(next_band % bands);
                        next_band += 1;
                        let _ = tx_reader.send(
                            rfb_proto::framebuffer_update_request_rect(bx, by, bw, bh, false),
                        );
                    } else if bands > 1 {
                        // Idle: fall back to cheap incremental polling.
                        thread::sleep(Duration::from_millis(16));
                        let _ = tx_reader
                            .send(framebuffer_update_request(fb_w, fb_h, true));
                    } else if pipeline > 1 {
                        // Keep the pipe full: one new request per update.
                        let _ = tx_reader
                            .send(framebuffer_update_request(fb_w, fb_h, true));
                    } else {
                        let idle_ms = if input_is_hot() { 4 } else { 16 };
                        thread::sleep(Duration::from_millis(idle_ms));
                        let _ = tx_reader
                            .send(framebuffer_update_request(fb_w, fb_h, true));
                    }
                }
                Err(e) => {
                    eprintln!("read error: {e:#}");
                    let _ = reader_proxy.send_event(UserEvent::Disconnected(format!("{e:#}")));
                    break;
                }
            }
        }
    });

    let mut app = App::new(
        &event_loop,
        fb_w,
        fb_h,
        input_w,
        input_h,
        tx_out,
        rx_upd,
        proxy,
        dump_path,
        dump_after,
        exit_after_dump,
        debug_input,
        ime,
        exit_after_secs.map(|s| Instant::now() + Duration::from_secs_f64(s)),
        demo_cursor,
        file_dest.clone(),
        pull_pending.clone(),
        remote_path.clone(),
        keymap,
        grab,
        scroll_test,
        frame_slot.clone(),
        coast,
    )?;
    event_loop.run_app(&mut app)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn us_table_positions_and_inverse() {
        assert_eq!(US.wire_keysym(KeyCode::KeyA, false), Some(0x61));
        assert_eq!(US.wire_keysym(KeyCode::KeyA, true), Some(0x41));
        assert_eq!(US.wire_keysym(KeyCode::Digit2, false), Some(0x32));
        assert_eq!(US.wire_keysym(KeyCode::Digit2, true), Some(0x40));
        assert_eq!(US.wire_keysym(KeyCode::BracketRight, false), Some(0x5d));
        assert_eq!(US.char_position('@'), Some((KeyCode::Digit2, true)));
        assert_eq!(US.char_position(']'), Some((KeyCode::BracketRight, false)));
        assert_eq!(US.char_position('€'), None);
    }

    #[test]
    fn jis_table_matches_the_measured_layout() {
        // Measured on the lab server: @ at BracketLeft, ^ at Equal, : at
        // Quote, [ at BracketRight, ] at Backslash.
        assert_eq!(JIS.wire_keysym(KeyCode::BracketLeft, false), Some(0x40));
        assert_eq!(JIS.wire_keysym(KeyCode::BracketLeft, true), Some(0x60));
        assert_eq!(JIS.wire_keysym(KeyCode::Equal, false), Some(0x5e));
        assert_eq!(JIS.wire_keysym(KeyCode::Equal, true), Some(0x7e));
        assert_eq!(JIS.wire_keysym(KeyCode::Quote, false), Some(0x3a));
        assert_eq!(JIS.wire_keysym(KeyCode::Quote, true), Some(0x2a));
        assert_eq!(JIS.wire_keysym(KeyCode::BracketRight, false), Some(0x5b));
        assert_eq!(JIS.wire_keysym(KeyCode::Backslash, false), Some(0x5d));
        assert_eq!(JIS.wire_keysym(KeyCode::Digit2, true), Some(0x22));
        assert_eq!(JIS.wire_keysym(KeyCode::Minus, true), Some(0x3d));
        assert_eq!(JIS.wire_keysym(KeyCode::Semicolon, true), Some(0x2b));
        // The inverse table is what a translate mode would use.
        assert_eq!(JIS.char_position('@'), Some((KeyCode::BracketLeft, false)));
        assert_eq!(JIS.char_position('~'), Some((KeyCode::Equal, true)));
        assert_eq!(JIS.char_position('"'), Some((KeyCode::Digit2, true)));
    }

    #[test]
    fn keypad_and_jis_input_keys() {
        // Keypad keys are positions, not digit characters.
        assert_eq!(US.special(KeyCode::Numpad0), Some(0xffb0));
        assert_eq!(US.special(KeyCode::NumpadEnter), Some(0xff8d));
        // The JIS input keys have no keysym in the daemon's tables, so they
        // must not be given one here; they are logged as unsupported.
        for code in [
            KeyCode::Lang1,
            KeyCode::Lang2,
            KeyCode::Convert,
            KeyCode::NonConvert,
            KeyCode::KanaMode,
        ] {
            assert_eq!(JIS.special(code), None);
            assert!(is_unsupported_jis_code(code));
        }
        // The Yen key is a character position, not a special keysym.
        assert_eq!(JIS.row(KeyCode::IntlYen), Some(('\\', '|')));
    }



    #[test]
    fn scroll_synthesis_is_portable_and_keeps_sub_line_motion() {
        let scroll = |dx: f32, dy: f32, phase: Phase, precise: bool| Gesture::Scroll {
            dx,
            dy,
            phase,
            precise,
            momentum: None,
        };
        let step = PIXELS_PER_LINE;
        // Precise (trackpad): line field zero, the whole delta in 16.16 fixed,
        // which is what the native client's 82-byte messages do.
        let (msg, _, lines, fixed) =
            scroll_message(&scroll(0.0, 2.0 * step, Phase::Changed, true), 10, 20, (0.0, 0.0));
        let msg = msg.unwrap();
        assert_eq!(msg.len(), 58);
        assert_eq!(lines, (0, 0), "precise input must not send whole lines");
        assert_eq!(fixed, (0, 2 * 65536));
        let dy = i16::from_be_bytes([msg[10], msg[11]]);
        assert_eq!(dy, 0);
        let fixed_y = i32::from_be_bytes([msg[18], msg[19], msg[20], msg[21]]);
        assert_eq!(fixed_y, 2 * 65536);
        // A sub-line sample still goes out, with no local accumulation.
        let (half, _, lines, fixed) =
            scroll_message(&scroll(0.0, step / 2.0, Phase::Changed, true), 0, 0, (0.0, 0.0));
        let half = half.unwrap();
        assert_eq!(lines, (0, 0));
        assert_eq!(fixed, (0, 32768));
        let fixed_y = i32::from_be_bytes([half[18], half[19], half[20], half[21]]);
        assert_eq!(fixed_y, 32768); // 0.5 line in 16.16
        // A wheel keeps the line units and sends no fixed delta.
        let (wheel, _, lines, fixed) =
            scroll_message(&scroll(0.0, 2.0 * step, Phase::Changed, false), 0, 0, (0.0, 0.0));
        assert!(wheel.is_some());
        assert_eq!(lines, (0, 2));
        assert_eq!(fixed, (0, 0));
        // Phase codes and momentum land in their fields.
        let (msg, _, _, _) = scroll_message(
            &scroll(0.0, PIXELS_PER_LINE, Phase::Ended, true),
            0,
            0,
            (0.0, 0.0),
        );
        let msg = msg.unwrap();
        let phase = u32::from_be_bytes([msg[46], msg[47], msg[48], msg[49]]);
        assert_eq!(phase, 4);
    }

    #[test]
    fn momentum_is_dropped_unless_coasting() {
        // Touchdown events always pass.
        assert!(scroll_forward(Some(false), false));
        assert!(scroll_forward(Some(false), true));
        // OS momentum is dropped by default and forwarded with --coast.
        assert!(!scroll_forward(Some(true), false));
        assert!(scroll_forward(Some(true), true));
        // Unclassified events are forwarded, never swallowed.
        assert!(scroll_forward(None, false));
    }

    #[test]
    fn usage_documents_every_flag() {
        // Every flag literal in the argument parser must appear in USAGE:
        // adding a parser arm without documenting it fails here.
        let src = include_str!("main.rs");
        let start = src
            .find("let args: Vec<String> = std::env::args")
            .expect("argument parser not found");
        let end = src[start..]
            .find("// Apple's client is push-based")
            .map(|offset| start + offset)
            .expect("end of the argument parser not found");
        let parser = &src[start..end];
        let mut missing = Vec::new();
        for line in parser.lines() {
            let mut rest = line;
            while let Some(start) = rest.find('"') {
                rest = &rest[start + 1..];
                let Some(end) = rest.find('"') else { break };
                let lit = &rest[..end];
                rest = &rest[end + 1..];
                let is_flag = lit.starts_with("--")
                    && lit.len() > 2
                    && lit[2..]
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c == '-');
                if is_flag && !USAGE.contains(lit) {
                    missing.push(lit.to_string());
                }
            }
        }
        missing.sort();
        missing.dedup();
        assert!(missing.is_empty(), "flags missing from USAGE: {missing:?}");
    }
    #[test]
    fn named_and_ime_keysyms_unchanged() {
        assert_eq!(named_keysym(&NamedKey::Enter), Some(0xff0d));
        assert_eq!(named_keysym(&NamedKey::ArrowLeft), Some(0xff51));
        assert_eq!(text_keysym('@'), 0x40);
        assert_eq!(text_keysym('あ'), 0x0100_3042);
    }
}
