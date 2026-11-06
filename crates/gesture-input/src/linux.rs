//! Linux evdev multitouch backend (MT protocol B).
//!
//! Requires read access to the trackpad's `/dev/input/event*` node. The
//! preferred permission is a scoped udev `uaccess` rule for
//! `ID_INPUT_TOUCHPAD=1`; the coarse alternative is the `input` group, which
//! grants every input device including the keyboard. Access is checked when a
//! device is opened, so a permission failure is reported as such, not as
//! "no device".
//!
//! Devices are discovered through udev (`ID_INPUT_TOUCHPAD=1`), never by node
//! number. We do not grab the device unless the caller explicitly asks.

use crate::{Gesture, GestureRecognizer, TouchPoint};
use std::ffi::CStr;
use std::fs::{self, File};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

const EV_SYN: u16 = 0;
const SYN_REPORT: u16 = 0;
const EV_ABS: u16 = 3;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MAX: u16 = 0x3f;

#[repr(C)]
#[derive(Clone, Copy)]
struct InputEvent {
    tv_sec: libc::time_t,
    tv_usec: libc::suseconds_t,
    type_: u16,
    code: u16,
    value: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputAbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

const IOC_READ: u32 = 2;
const fn ioc(dir: u32, ty: u8, nr: u8, size: usize) -> libc::c_ulong {
    ((dir << 30) | ((size as u32) << 16) | ((ty as u32) << 8) | nr as u32) as libc::c_ulong
}
fn eviocgname(len: usize) -> libc::c_ulong {
    ioc(IOC_READ, b'E', 0x06, len)
}
fn eviocgbit(ev: u16, len: usize) -> libc::c_ulong {
    ioc(IOC_READ, b'E', 0x20 + ev as u8, len)
}
fn eviocgabs(abs: u16) -> libc::c_ulong {
    ioc(
        IOC_READ,
        b'E',
        0x40 + abs as u8,
        std::mem::size_of::<InputAbsInfo>(),
    )
}

#[derive(Debug, Clone)]
pub struct Touchpad {
    pub path: PathBuf,
    pub name: String,
    pub x_min: i32,
    pub x_max: i32,
    pub y_min: i32,
    pub y_max: i32,
}

/// A discovered device, as reported to the caller before the reader starts.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub path: PathBuf,
    pub name: String,
}

impl std::fmt::Display for DeviceInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.name, self.path.display())
    }
}

fn has_bit(bits: &[u8], code: u16) -> bool {
    let i = (code / 8) as usize;
    i < bits.len() && (bits[i] & (1 << (code % 8))) != 0
}

fn probe(path: &Path) -> io::Result<Touchpad> {
    let file = File::open(path)?;
    let fd = file.as_raw_fd();

    let mut name_buf = [0u8; 256];
    let rc = unsafe { libc::ioctl(fd, eviocgname(name_buf.len()), name_buf.as_mut_ptr()) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    let name = unsafe { CStr::from_ptr(name_buf.as_ptr() as *const libc::c_char) }
        .to_string_lossy()
        .to_string();

    let mut ev_bits = [0u8; 8];
    let rc = unsafe { libc::ioctl(fd, eviocgbit(0, ev_bits.len()), ev_bits.as_mut_ptr()) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    if !has_bit(&ev_bits, EV_ABS) {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no EV_ABS"));
    }

    let mut abs_bits = [0u8; (ABS_MAX as usize / 8) + 1];
    let rc = unsafe {
        libc::ioctl(
            fd,
            eviocgbit(EV_ABS, abs_bits.len()),
            abs_bits.as_mut_ptr(),
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    if !has_bit(&abs_bits, ABS_MT_POSITION_X) && !has_bit(&abs_bits, ABS_MT_SLOT) {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no MT positions"));
    }

    let mut xi = InputAbsInfo::default();
    let mut yi = InputAbsInfo::default();
    let rc = unsafe { libc::ioctl(fd, eviocgabs(ABS_MT_POSITION_X), &mut xi) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    let rc = unsafe { libc::ioctl(fd, eviocgabs(ABS_MT_POSITION_Y), &mut yi) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    if xi.maximum <= xi.minimum || yi.maximum <= yi.minimum {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad axis range"));
    }

    Ok(Touchpad {
        path: path.to_path_buf(),
        name,
        x_min: xi.minimum,
        x_max: xi.maximum,
        y_min: yi.minimum,
        y_max: yi.maximum,
    })
}

/// True when udev's property block marks the node as a touchpad.
fn is_touchpad_props(props: &str) -> bool {
    props
        .lines()
        .any(|line| line.trim() == "ID_INPUT_TOUCHPAD=1")
}

fn udev_properties(path: &Path) -> io::Result<String> {
    let out = Command::new("udevadm")
        .arg("info")
        .arg("--query=property")
        .arg("--name")
        .arg(path)
        .output()?;
    if !out.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("udevadm info failed for {}", path.display()),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn permission_help(name: &str, path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "touchpad {name} ({}) is present but cannot be opened: permission denied.\n\
             Grant access to this device only with a udev uaccess rule (preferred):\n\
             /etc/udev/rules.d/69-ss-clone-input.rules\n\
             SUBSYSTEM==\"input\", KERNEL==\"event*\", ENV{{ID_INPUT_TOUCHPAD}}==\"1\", TAG+=\"uaccess\"\n\
             then: sudo udevadm control --reload-rules && sudo udevadm trigger --subsystem-match=input\n\
             Coarse alternative (grants every input device, including every keyboard): add this\n\
             user to the `input` group and start a new login session.",
            path.display()
        ),
    )
}

/// Discover touchpads through udev. Never hardcodes `eventN`.
///
/// A touchpad whose node exists but cannot be opened yields
/// `PermissionDenied` with both remedies in the message; a genuinely absent
/// device yields `NotFound`. Probe errors for non-touchpad nodes are ignored.
pub fn list_touchpads() -> io::Result<Vec<Touchpad>> {
    let mut pads = Vec::new();
    let mut denied: Vec<(PathBuf, String)> = Vec::new();
    let mut udev_missing = false;

    for entry in fs::read_dir("/sys/class/input")? {
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
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                udev_missing = true;
                continue;
            }
            Err(_) => continue,
        };
        if !is_touchpad_props(&props) {
            continue;
        }
        match probe(&dev) {
            Ok(pad) => pads.push(pad),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                let sys_name = entry.path().join("device/name");
                let name = fs::read_to_string(sys_name)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|_| name.clone());
                denied.push((dev, name));
            }
            Err(_) => {}
        }
    }

    if pads.is_empty() {
        if let Some((path, name)) = denied.into_iter().next() {
            return Err(permission_help(&name, &path));
        }
        if udev_missing {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no touchpad found: `udevadm` is required to identify ID_INPUT_TOUCHPAD=1 nodes",
            ));
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no touchpad found: no /dev/input/event* node has ID_INPUT_TOUCHPAD=1",
        ));
    }

    pads.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(pads)
}

/// Spawn a reader thread for the first discovered touchpad.
/// Returns the devices that were found (one is read).
pub fn spawn_default<F>(mut on_gesture: F, verbose: bool) -> io::Result<Vec<DeviceInfo>>
where
    F: FnMut(Gesture) + Send + 'static,
{
    let pads = list_touchpads()?;
    let infos = pads
        .iter()
        .map(|p| DeviceInfo {
            path: p.path.clone(),
            name: p.name.clone(),
        })
        .collect();
    let pad = pads.into_iter().next().unwrap();
    thread::Builder::new()
        .name("evdev-gestures".into())
        .spawn(move || {
            if let Err(e) = run(pad, &mut on_gesture, verbose) {
                eprintln!("evdev gesture reader: {e}");
            }
        })?;
    Ok(infos)
}

fn run<F>(pad: Touchpad, on_gesture: &mut F, verbose: bool) -> io::Result<()>
where
    F: FnMut(Gesture),
{
    let file = File::open(&pad.path)?;
    let fd = file.as_raw_fd();
    if verbose {
        eprintln!("evdev: opened {} ({})", pad.name, pad.path.display());
    }
    let mut slots: [Option<(i32, i32, i32)>; 16] = [None; 16]; // (id, raw x, raw y)
    let mut cur_slot = 0usize;
    let mut recognizer = GestureRecognizer::new();
    let mut buf = [0u8; std::mem::size_of::<InputEvent>() * 64];
    let mut frames = 0u64;

    loop {
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
        let count = n as usize / std::mem::size_of::<InputEvent>();
        for i in 0..count {
            let off = i * std::mem::size_of::<InputEvent>();
            let ev: InputEvent =
                unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const InputEvent) };
            match ev.type_ {
                EV_ABS => match ev.code {
                    ABS_MT_SLOT => {
                        cur_slot = (ev.value.max(0) as usize).min(slots.len() - 1);
                    }
                    ABS_MT_TRACKING_ID => {
                        if ev.value < 0 {
                            slots[cur_slot] = None;
                        } else {
                            let (x, y) = slots[cur_slot]
                                .map(|s| (s.1, s.2))
                                .unwrap_or((pad.x_min, pad.y_min));
                            slots[cur_slot] = Some((ev.value, x, y));
                        }
                    }
                    ABS_MT_POSITION_X => {
                        let mut s = slots[cur_slot].unwrap_or((0, pad.x_min, pad.y_min));
                        s.1 = ev.value;
                        slots[cur_slot] = Some(s);
                    }
                    ABS_MT_POSITION_Y => {
                        let mut s = slots[cur_slot].unwrap_or((0, pad.x_min, pad.y_min));
                        s.2 = ev.value;
                        slots[cur_slot] = Some(s);
                    }
                    _ => {}
                },
                EV_SYN if ev.code == SYN_REPORT => {
                    let xr = (pad.x_max - pad.x_min) as f32;
                    let yr = (pad.y_max - pad.y_min) as f32;
                    let points: Vec<TouchPoint> = slots
                        .iter()
                        .flatten()
                        .filter(|(id, _, _)| *id >= 0)
                        .map(|(id, x, y)| TouchPoint {
                            id: *id,
                            x: (*x - pad.x_min) as f32 / xr,
                            y: (*y - pad.y_min) as f32 / yr,
                        })
                        .collect();
                    frames += 1;
                    if verbose && frames % 240 == 0 {
                        eprintln!("evdev: {} frames from {}", frames, pad.name);
                    }
                    for g in recognizer.process(&points) {
                        on_gesture(g);
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_touchpad_props;

    #[test]
    fn udev_property_identifies_a_touchpad() {
        let props = "DEVPATH=/devices/pci0000:00/event10\n\
                     ID_INPUT=1\n\
                     ID_INPUT_TOUCHPAD=1\n\
                     ID_INPUT_TOUCHPAD_INTEGRATION=internal\n";
        assert!(is_touchpad_props(props));
    }

    #[test]
    fn udev_property_does_not_treat_keyboard_or_zero_as_touchpad() {
        assert!(!is_touchpad_props("ID_INPUT_KEYBOARD=1\nID_INPUT=1\n"));
        assert!(!is_touchpad_props("ID_INPUT_TOUCHPAD=0\n"));
        assert!(!is_touchpad_props("XID_INPUT_TOUCHPAD=1\n"));
    }
}
