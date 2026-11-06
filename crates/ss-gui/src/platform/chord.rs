//! Release-chord parsing, independent of any OS API.
//!
//! The chord spec is macOS keycodes today (`--release-grab cmd+shift`), but the
//! parser itself moved out of the `CGEventTap` module so it is portable and so
//! the Linux evdev grab can reuse it.

use std::time::Duration;

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

/// Modifier flags for the explicit local→remote paste chord.
///
/// Kept as plain booleans, not winit types, so the matcher is portable and
/// testable without a window system.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PasteModifiers {
    pub control: bool,
    pub shift: bool,
    pub alt: bool,
    pub command: bool,
}

/// Ctrl+V with no other modifier: the chord that pastes the local clipboard on
/// the remote.
///
/// SUPER+V cannot be used on Wayland: compositors bind it before the client
/// sees it (Hyprland binds both `SUPER+V` universal paste and
/// `SUPER+SHIFT+V` clipboard manager), and a Linux client has no keyboard
/// grab to take it back. Ctrl+V is the chord the compositor leaves alone, and
/// the client translates it into the remote's own Cmd+V. Excluding the other
/// modifiers keeps Ctrl+Shift+V (pull file) and the Ctrl+Alt gesture-test
/// shortcuts untouched.
pub fn is_paste_chord(m: PasteModifiers) -> bool {
    m.control && !m.shift && !m.alt && !m.command
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

    #[test]
    fn paste_chord_is_ctrl_v_alone() {
        let ctrl = PasteModifiers {
            control: true,
            ..PasteModifiers::default()
        };
        assert!(is_paste_chord(ctrl));
        // Every other modifier must disqualify the chord, so the existing
        // Ctrl+Shift+V pull and Ctrl+Alt gesture shortcuts keep their paths.
        for extra in [
            PasteModifiers { shift: true, ..ctrl },
            PasteModifiers { alt: true, ..ctrl },
            PasteModifiers { command: true, ..ctrl },
        ] {
            assert!(!is_paste_chord(extra), "{extra:?} must not paste");
        }
        assert!(!is_paste_chord(PasteModifiers::default()));
        assert!(!is_paste_chord(PasteModifiers {
            shift: true,
            ..PasteModifiers::default()
        }));
    }
}
