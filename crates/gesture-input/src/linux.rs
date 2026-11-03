//! Linux evdev multitouch backend (MT protocol B).
//!
//! Requires read access to `/dev/input/event*`. Add the user to the `input`
//! group (or run with sufficient privileges) and pass `--evdev` to the GUI.
//! We do not grab the device by default so the compositor keeps working.

use crate::{Gesture, GestureRecognizer, TouchPoint};
use std::ffi::CStr;
use std::fs::{self, File};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
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

pub fn list_touchpads() -> io::Result<Vec<Touchpad>> {
    let mut out = Vec::new();
    for entry in fs::read_dir("/dev/input")? {
        let path = entry?.path();
        let is_event = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.starts_with("event"))
            .unwrap_or(false);
        if !is_event {
            continue;
        }
        if let Ok(tp) = probe(&path) {
            out.push(tp);
        }
    }
    Ok(out)
}

/// Spawn a reader thread for the first multitouch device.
/// Returns the device names that were found.
pub fn spawn_default<F>(mut on_gesture: F) -> io::Result<Vec<String>>
where
    F: FnMut(Gesture) + Send + 'static,
{
    let pads = list_touchpads()?;
    if pads.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no multitouch device found under /dev/input (need input group access)",
        ));
    }
    let names = pads.iter().map(|p| p.name.clone()).collect();
    let pad = pads.into_iter().next().unwrap();
    thread::Builder::new()
        .name("evdev-gestures".into())
        .spawn(move || {
            if let Err(e) = run(pad, &mut on_gesture) {
                eprintln!("evdev gesture reader: {e}");
            }
        })?;
    Ok(names)
}

fn run<F>(pad: Touchpad, on_gesture: &mut F) -> io::Result<()>
where
    F: FnMut(Gesture),
{
    let file = File::open(&pad.path)?;
    let fd = file.as_raw_fd();
    let mut slots: [Option<(i32, i32, i32)>; 16] = [None; 16]; // (id, raw x, raw y)
    let mut cur_slot = 0usize;
    let mut recognizer = GestureRecognizer::new();
    let mut buf = [0u8; std::mem::size_of::<InputEvent>() * 64];

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
                    for g in recognizer.process(&points) {
                        on_gesture(g);
                    }
                }
                _ => {}
            }
        }
    }
}
