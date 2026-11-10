//! Linux evdev keyboard grab for the explicit `--grab-keyboard` mode.
//!
//! A keyboard grab is only safe with a release path that does not depend on the
//! compositor. The release chord is recognised here from the raw evdev stream
//! (Ctrl+Alt+Esc held 350 ms), so the compositor never has to see it.
//! All key-up/modifier-release wire messages are sent by the GUI through the
//! `on_release` callback before this module drops the grab, including when the
//! watchdog, not the UI, is the one releasing.
//!
//! Raw keys are never logged: this fd can see passwords. Only device identity,
//! frame/key counts and the fact that the chord fired are logged.

use super::{
    is_touchpad_props, now_ms, udev_properties, DeviceEvent, DeviceHandle, DeviceInfo,
    DeviceInner,
};
use crate::chord::{ReleaseChord, LINUX_KEY_ESC};
use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

const EV_KEY: u16 = 1;

pub struct KeyboardDevice {
    pub info: DeviceInfo,
    pub handle: DeviceHandle,
}

fn is_keyboard_props(props: &str) -> bool {
    props.lines().any(|line| line.trim() == "ID_INPUT_KEYBOARD=1")
}

fn is_apple_internal(props: &str) -> bool {
    props.lines().any(|line| line.trim() == "ID_VENDOR_ID=05ac")
        && props
            .lines()
            .any(|line| line.trim() == "ID_MODEL_ID=0274")
}

fn keyboard_permission_help(name: &str, path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "keyboard {name} ({}) is present but cannot be opened: permission denied.\n\
             Grant access to the Apple internal keyboard only with a udev uaccess rule:\n\
             /etc/udev/rules.d/69-openssc-input.rules\n\
             SUBSYSTEM==\"input\", KERNEL==\"event*\", ENV{{ID_INPUT_KEYBOARD}}==\"1\", ENV{{ID_MODEL_ID}}==\"0274\", ENV{{ID_VENDOR_ID}}==\"05ac\", TAG+=\"uaccess\"\n\
             then: sudo udevadm control --reload-rules && sudo udevadm trigger --subsystem-match=input\n\
             Do not add the user to the `input` group: that exposes every keyboard.",
            path.display()
        ),
    )
}

fn list_keyboards() -> io::Result<Vec<(PathBuf, String, bool)>> {
    let mut out = Vec::new();
    let mut denied = Vec::new();
    for entry in std::fs::read_dir("/sys/class/input")? {
        let entry = entry?;
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(name) if name.starts_with("event") => name.to_string(),
            _ => continue,
        };
        let dev = PathBuf::from("/dev/input").join(&name);
        if !dev.exists() {
            continue;
        }
        let props = match udev_properties(&dev) {
            Ok(props) => props,
            Err(_) => continue,
        };
        if !is_keyboard_props(&props) || is_touchpad_props(&props) {
            continue;
        }
        let sys_name = entry.path().join("device/name");
        let name = std::fs::read_to_string(sys_name)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| name.clone());
        match File::open(&dev) {
            Ok(_) => out.push((dev, name, is_apple_internal(&props))),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                denied.push((dev, name));
            }
            Err(_) => {}
        }
    }
    if out.is_empty() {
        if let Some((path, name)) = denied.into_iter().next() {
            return Err(keyboard_permission_help(&name, &path));
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no keyboard found: no /dev/input/event* node has ID_INPUT_KEYBOARD=1",
        ));
    }
    // Prefer the internal Apple keyboard the uaccess rule names.
    out.sort_by_key(|(_, _, apple)| !*apple);
    Ok(out)
}

pub fn spawn_keyboard<F>(
    mut on_event: F,
    verbose: bool,
    on_release: Arc<dyn Fn() + Send + Sync>,
) -> io::Result<KeyboardDevice>
where
    F: FnMut(DeviceEvent) + Send + 'static,
{
    let keyboards = list_keyboards()?;
    let (path, name, _) = keyboards.into_iter().next().unwrap();
    let file = Arc::new(File::open(&path).map_err(|e| {
        if e.kind() == io::ErrorKind::PermissionDenied {
            keyboard_permission_help(&name, &path)
        } else {
            e
        }
    })?);
    let grabbed = Arc::new(AtomicBool::new(false));
    let inner = Arc::new(DeviceInner {
        file: file.clone(),
        name: name.clone(),
        path: path.clone(),
        grabbed: grabbed.clone(),
        last_beat_ms: AtomicU64::new(now_ms()),
        watchdog_started: AtomicBool::new(false),
        role: "keyboard",
        on_release: Some(on_release),
    });
    let handle = DeviceHandle { inner };
    let info = DeviceInfo { path, name };
    let info_for_thread = info.clone();
    thread::Builder::new()
        .name("evdev-keyboard".into())
        .spawn(move || {
            if verbose {
                eprintln!("evdev keyboard: opened {}", info_for_thread);
            }
            if let Err(e) = run(file, grabbed, &mut on_event, verbose) {
                eprintln!("evdev keyboard reader: {e}");
            }
        })?;
    Ok(KeyboardDevice { info, handle })
}

fn run<F>(
    file: Arc<File>,
    grabbed: Arc<AtomicBool>,
    on_event: &mut F,
    verbose: bool,
) -> io::Result<()>
where
    F: FnMut(DeviceEvent),
{
    let fd = file.as_raw_fd();
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let mut chord = ReleaseChord::new();
    let mut buf = [0u8; std::mem::size_of::<super::InputEvent>() * 64];
    let mut keys = 0u64;
    let mut released = false;

    loop {
        if !released && !grabbed.load(Ordering::Relaxed) {
            // A blur/release may have happened mid-chord; start clean when the
            // next focus grabs again.
            chord.reset();
        }
        let rc = unsafe { libc::poll(&mut pollfd, 1, 25) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if rc > 0 {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if n == 0 {
                return Ok(());
            }
            let count = n as usize / std::mem::size_of::<super::InputEvent>();
            for i in 0..count {
                let off = i * std::mem::size_of::<super::InputEvent>();
                let ev: super::InputEvent = unsafe {
                    std::ptr::read_unaligned(buf.as_ptr().add(off) as *const super::InputEvent)
                };
                if ev.type_ != EV_KEY || released {
                    continue;
                }
                if !grabbed.load(Ordering::Relaxed) {
                    continue;
                }
                keys += 1;
                if verbose && keys % 240 == 0 {
                    eprintln!("evdev keyboard: {keys} keys");
                }
                let down = ev.value != 0;
                let repeat = ev.value == 2;
                let was_pending = chord.pending_hold();
                let step = chord.on_key(ev.code, down, Instant::now());
                if chord.pending_hold() && !was_pending {
                    eprintln!("evdev keyboard: chord armed (ctrl+alt+esc held)");
                } else if was_pending && !chord.pending_hold() && !chord.completed() {
                    eprintln!("evdev keyboard: chord cancelled (early release or another key)");
                }
                if step.replay_esc_down {
                    on_event(DeviceEvent::Key {
                        code: LINUX_KEY_ESC,
                        down: true,
                        repeat: false,
                    });
                }
                if step.replay_esc_up {
                    on_event(DeviceEvent::Key {
                        code: LINUX_KEY_ESC,
                        down: false,
                        repeat: false,
                    });
                }
                if !step.consume {
                    on_event(DeviceEvent::Key {
                        code: ev.code,
                        down,
                        repeat,
                    });
                }
            }
        }

        if !released && grabbed.load(Ordering::Relaxed) && chord.tick(Instant::now()) {
            eprintln!("evdev keyboard: chord completed");
            on_event(DeviceEvent::ReleaseChord);
            released = true;
        }
        if released && !grabbed.load(Ordering::Relaxed) {
            // The GUI released after the chord. Stay alive so the next focus
            // can re-arm the same device; a dead reader would leave a future
            // grab with no one to read it or recognise the escape chord.
            released = false;
            chord.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_keyboard_props;

    #[test]
    fn udev_property_identifies_a_keyboard_not_a_touchpad() {
        assert!(is_keyboard_props("ID_INPUT_KEYBOARD=1\nID_INPUT=1\n"));
        assert!(!is_keyboard_props("ID_INPUT_TOUCHPAD=1\n"));
        assert!(!is_keyboard_props("ID_INPUT_KEYBOARD=0\n"));
    }
}
