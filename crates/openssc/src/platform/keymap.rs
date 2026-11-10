//! macOS keycode and modifier tables used by the event tap.
//!
//! These are pure data: no OS calls, so the same code compiles everywhere. They
//! live outside the grab implementation because the app's event handling and
//! the tap must agree on which physical key and modifier state an event means.

// CGEventFlags (device-independent).
pub(crate) const FLAG_SHIFT: u64 = 0x0002_0000;
pub(crate) const FLAG_CONTROL: u64 = 0x0004_0000;
pub(crate) const FLAG_OPTION: u64 = 0x0008_0000;
pub(crate) const FLAG_COMMAND: u64 = 0x0010_0000;
#[cfg(target_os = "macos")]
pub(crate) const FLAG_FN: u64 = 0x0080_0000;
#[cfg(target_os = "macos")]
pub(crate) const FLAG_CAPS: u64 = 0x0001_0000;
// Device-dependent left/right bits (IOLLEvent.h).
#[cfg(target_os = "macos")]
pub(crate) const DEV_LEFT_SHIFT: u64 = 0x0000_0002;
#[cfg(target_os = "macos")]
pub(crate) const DEV_LEFT_CMD: u64 = 0x0000_0008;

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
