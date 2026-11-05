//! Live Apple Screen Sharing client window.
//!
//! Runs on macOS for lab testing and on Linux/Wayland via winit + softbuffer.
//! The protocol crate is platform-neutral; this file only translates window
//! events into RFB/Apple messages and presents BGRX frames.

use anyhow::Result;
use gesture_input::{Gesture, Phase};
use rfb_proto::mvs::{MvsDecoder, Rect};
use rfb_proto::{
    client_cut_text, clipboard_fetch, framebuffer_update_request, gesture_begin_end,
    gesture_magnify, gesture_mouse_scroll, gesture_rotate, gesture_smart_magnify, gesture_swipe,
    key_event, image_item, pasteboard_archive, pasteboard_image, pasteboard_text,
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
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use raw_window_handle::{DisplayHandle, HasDisplayHandle};

mod filecopy;

fn keysym_for(c: char) -> Option<(u32, bool)> {
    // Returns (keysym, needs_shift). Non-ASCII uses the X11 Unicode keysym
    // convention (0x01000000 | codepoint).
    if !c.is_ascii() {
        return Some((0x0100_0000 | c as u32, false));
    }
    if c.is_ascii_lowercase() || c.is_ascii_digit() {
        return Some((c as u32, false));
    }
    if c.is_ascii_uppercase() {
        return Some(((c.to_ascii_lowercase()) as u32, true));
    }
    let shifted = match c {
        '!' => ('1', true),
        '@' => ('2', true),
        '#' => ('3', true),
        '$' => ('4', true),
        '%' => ('5', true),
        '^' => ('6', true),
        '&' => ('7', true),
        '*' => ('8', true),
        '(' => ('9', true),
        ')' => ('0', true),
        '_' => ('-', true),
        '+' => ('=', true),
        '{' => ('[', true),
        '}' => (']', true),
        '|' => ('\\', true),
        ':' => (';', true),
        '"' => ('\'', true),
        '<' => (',', true),
        '>' => ('.', true),
        '?' => ('/', true),
        '~' => ('`', true),
        _ => (c, false),
    };
    Some((shifted.0 as u32, shifted.1))
}

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
        let _ = std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .status();
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
                    0.0,
                    0.0,
                    0.0,
                    0,
                    0,
                    px,
                    py,
                ));
                out.push(gesture_begin_end(2, 0, px, py));
            }
        }
        Gesture::Scroll { dx, dy, phase: _ } => {
            // Normalized trackpad motion (fraction of the pad) -> scroll lines.
            // The scale is a feel constant, to be calibrated on a Linux pad.
            const SCROLL_LINES_PER_UNIT: f32 = 600.0;
            let lx = (dx * SCROLL_LINES_PER_UNIT).clamp(-40.0, 40.0) as i16;
            let ly = (dy * SCROLL_LINES_PER_UNIT).clamp(-40.0, 40.0) as i16;
            if lx != 0 || ly != 0 {
                out.push(gesture_mouse_scroll(
                    lx,
                    ly,
                    0,
                    (0, 0, 0),
                    (0, 0, 0),
                    2,
                    0,
                    fb_w / 2,
                    fb_h / 2,
                ));
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
}

struct App {
    context: softbuffer::Context<DisplayHandle<'static>>,
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<DisplayHandle<'static>, Arc<Window>>>,
    fb: Vec<u32>,
    fb_w: u16,
    fb_h: u16,
    /// Input coordinates stay in the server's original (backing) space even
    /// when the framebuffer image is scaled down.
    input_w: u16,
    input_h: u16,
    tx: Sender<Vec<u8>>,
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
    /// Remote destination folder for dropped files (`/Users/<user>/Downloads`).
    file_dest: String,
    /// `--demo-cursor`: scripted remote pointer tour to exercise cursor shapes.
    demo_cursor: bool,
    demo_step: usize,
    demo_next: Option<Instant>,
    demo_dump_at: Option<(Instant, String)>,
    buttons: u8,
    dirty: bool,
    modifiers: ModifiersState,
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
    scroll_acc: (f32, f32),
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
    /// MultiVariant (0x3f3) decoder; `Some` when `--mvs` advertised it.
    mvs: Option<MvsDecoder>,
    /// BGRX canvas the MVS decoder paints into before we convert to words.
    mvs_bytes: Vec<u8>,
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
        _proxy: EventLoopProxy<UserEvent>,
        dump_path: Option<String>,
        dump_after: u32,
        exit_after_dump: bool,
        debug_input: bool,
        ime: bool,
        deadline: Option<Instant>,
        mvs: bool,
        demo_cursor: bool,
        file_dest: String,
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
            fb_w,
            fb_h,
            input_w,
            input_h,
            tx,
            rx,
            cursor: (input_w / 2, input_h / 2),
            cursor_shape: None,
            point_size: None,
            pointer_inside: true,
            cursor_hidden: false,
            file_dest,
            demo_cursor,
            demo_step: 0,
            demo_next: Some(Instant::now() + Duration::from_millis(600)),
            demo_dump_at: None,
            buttons: 0,
            dirty: true,
            modifiers: ModifiersState::empty(),
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
            scroll_acc: (0.0, 0.0),
            pinch_active: false,
            pinch_last: None,
            rotate_active: false,
            rotate_last: None,
            pinch_x: 0,
            pinch_y: 0,
            lat: Vec::new(),
            lat_next_report: 50,
            mvs: mvs.then(|| MvsDecoder::new(fb_w, fb_h)),
            mvs_bytes: Vec::new(),
        })
    }

    fn send(&self, body: Vec<u8>) {
        if self.tx.send(body).is_err() && self.debug_input {
            eprintln!("input channel closed (writer thread exited)");
        }
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
    fn queue_scroll(&mut self, dx: f32, dy: f32) {
        self.scroll_acc.0 += dx;
        self.scroll_acc.1 += dy;
        let lines_x = self.scroll_acc.0.trunc();
        let lines_y = self.scroll_acc.1.trunc();
        if lines_x == 0.0 && lines_y == 0.0 {
            return;
        }
        self.scroll_acc.0 -= lines_x;
        self.scroll_acc.1 -= lines_y;
        // Stay inside i16 and avoid one absurd event after a burst.
        let lx = lines_x.clamp(-40.0, 40.0) as i16;
        let ly = lines_y.clamp(-40.0, 40.0) as i16;
        let (x, y) = self.cursor;
        if self.debug_input {
            eprintln!("scroll lines=({lx},{ly}) at ({x},{y})");
        }
        self.send(gesture_mouse_scroll(
            lx,
            ly,
            0,
            (0, 0, 0),
            (0, 0, 0),
            2,
            0,
            x,
            y,
        ));
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
    fn apply_mvs(&mut self, r: &rfb_proto::RectUpdate) {
        if self.mvs.is_none() {
            return;
        }
        // Grow if the server ignored our scale request (full-size rect).
        if r.x == 0 && r.y == 0 && (r.w > self.fb_w || r.h > self.fb_h) {
            self.fb_w = r.w;
            self.fb_h = r.h;
            self.fb = vec![0u32; r.w as usize * r.h as usize];
            self.mvs_bytes.clear();
            if let Some(dec) = self.mvs.as_mut() {
                // Keep the quantisation tables learned from type-2 bodies.
                dec.resize(r.w, r.h);
            }
        }
        let need = self.fb_w as usize * self.fb_h as usize * 4;
        if self.mvs_bytes.len() != need {
            self.mvs_bytes = vec![0u8; need];
        }
        let rect = Rect::new(r.x, r.y, r.w, r.h);
        let debug = self.debug_input;
        let body_type = r.pixels.first().copied().unwrap_or(255);
        let applied = {
            // Disjoint field borrows: the decoder paints into mvs_bytes.
            let Self { mvs, mvs_bytes, .. } = self;
            let Some(dec) = mvs.as_mut() else { return };
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
                                "[{:6} ms] t0 {}x{}+{}+{} applied: {} cmds, {} dct, {} bits",
                                now_ms(),
                                r.w,
                                r.h,
                                r.x,
                                r.y,
                                stats.commands,
                                stats.dct_blocks,
                                stats.dct_bits
                            );
                        }
                    }
                    true
                }
                Ok(None) => false, // quant table update
                Err(e) => {
                    // Never kill the session for one bad body.
                    eprintln!(
                        "mvs body t{body_type} {}x{}+{}+{} failed: {e:#}",
                        r.w, r.h, r.x, r.y
                    );
                    false
                }
            }
        };
        if !applied {
            return;
        }
        let stride = self.fb_w as usize * 4;
        let width = self.fb_w as usize;
        let x = r.x as usize;
        let y0 = r.y as usize;
        for row in 0..r.h as usize {
            let dy = y0 + row;
            if dy >= self.fb_h as usize || x + r.w as usize > width {
                continue;
            }
            let src = dy * stride + x * 4;
            let dst = dy * width + x;
            let out = &mut self.fb[dst..dst + r.w as usize];
            for (slot, chunk) in out
                .iter_mut()
                .zip(self.mvs_bytes[src..src + r.w as usize * 4].chunks_exact(4))
            {
                *slot = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], 0]);
            }
        }
        self.dirty = true;
    }

    /// `--debug-input`: close the oldest pending input once a rect is painted.
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
            if !self.cursor_hidden {
                if let Some(w) = &self.window {
                    // The decoded bitmap replaces the OS pointer. This is
                    // supported on macOS (invisible cursor) and on Wayland/X11.
                    w.set_cursor_visible(false);
                    self.cursor_hidden = true;
                }
            }
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
        for r in update.rects {
            if r.encoding == 0x3f3 {
                self.apply_mvs(&r);
                continue;
            }
            if r.pixels.is_empty() || r.w == 0 || r.h == 0 {
                continue;
            }
            // If the server did not honour our scale request, grow to the
            // actual framebuffer geometry instead of dropping the frame.
            if r.x == 0
                && r.y == 0
                && (r.w > self.fb_w || r.h > self.fb_h)
                && r.pixels.len() == r.w as usize * r.h as usize * 4
            {
                self.fb_w = r.w;
                self.fb_h = r.h;
                self.fb = vec![0u32; r.w as usize * r.h as usize];
            }
            let dst_w = self.fb_w as usize;
            let src_row = r.w as usize * 4;
            let x = r.x as usize;
            let y = r.y as usize;
            for row in 0..r.h as usize {
                let dy = y + row;
                if dy >= self.fb_h as usize || x + r.w as usize > dst_w {
                    continue;
                }
                let src = row * src_row;
                if src + src_row > r.pixels.len() {
                    break;
                }
                let dst = dy * dst_w + x;
                // Convert BGRX bytes to words once, here, so presentation is a
                // straight memcpy per row.
                let out = &mut self.fb[dst..dst + r.w as usize];
                for (px, chunk) in out.iter_mut().zip(r.pixels[src..src + src_row].chunks_exact(4)) {
                    *px = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], 0]);
                }
            }
            self.dirty = true;
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
                    if let Some((ks, _)) = keysym_for(c) {
                        self.send(key_event(ks, true));
                        self.send(key_event(ks, false));
                    }
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
            WindowEvent::MouseWheel { delta, .. } => {
                note_input();
                // Trackpads report pixel deltas on Wayland, line deltas on
                // macOS. Convert both to scroll *lines*; the server only
                // honours the line-delta field (see queue_scroll).
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x, y),
                    MouseScrollDelta::PixelDelta(p) => {
                        (p.x as f32 / 12.0, p.y as f32 / 12.0)
                    }
                };
                self.queue_scroll(dx, dy);
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
                self.queue_scroll(delta.x / 60.0, delta.y / 60.0);
            }
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                note_input();
                let pressed = event.state == ElementState::Pressed;
                if event.repeat {
                    return;
                }
                if self.debug_input {
                    eprintln!(
                        "key {:?} logical={:?} text={:?}",
                        event.state, event.logical_key, event.text
                    );
                }
                // Test shortcuts for gesture calibration on macOS: Ctrl+Alt+arrows
                // emit a swipe; Ctrl+Alt+=/- emit a magnify.
                if self.modifiers.control_key() && self.modifiers.alt_key() && pressed {
                    match &event.logical_key {
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
                match &event.logical_key {
                    Key::Named(named) => {
                        if let Some(ks) = named_keysym(named) {
                            self.send(key_event(ks, pressed));
                        }
                    }
                    Key::Character(s) => {
                        // With IME enabled, composed text arrives via Ime::Commit.
                        if self.ime {
                            return;
                        }
                        for c in s.chars() {
                            if let Some((ks, shifted)) = keysym_for(c) {
                                if shifted && pressed {
                                    self.send(key_event(0xffe1, true));
                                }
                                self.send(key_event(ks, pressed));
                                if shifted && !pressed {
                                    self.send(key_event(0xffe1, false));
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
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
        if self.rotate_active
            && self.rotate_last.map_or(true, |t| t.elapsed() > Duration::from_millis(150))
        {
            self.send(gesture_rotate(0.0, 4, 31, self.pinch_x, self.pinch_y));
            self.rotate_active = false;
        }
        let mut got_pixels = false;
        while let Ok(update) = self.rx.try_recv() {
            let had_pixels = update.rects.iter().any(|r| !r.pixels.is_empty());
            let full = update.rects.iter().any(|r| {
                !r.pixels.is_empty()
                    && r.x == 0
                    && r.y == 0
                    && r.w == self.fb_w
                    && r.h == self.fb_h
            });
            self.apply_update(update);
            if had_pixels {
                self.frames += 1;
                got_pixels = true;
                LAST_APPLIED_MS.store(now_ms(), Ordering::Relaxed);
                PUSH_PAINTED.store(true, Ordering::Relaxed);
                self.record_input_latency();
                if self.frames >= self.lat_next_report {
                    self.report_input_latency("latency report");
                    self.lat_next_report = self.frames + 50;
                }
            }
            if full {
                self.full_frame = true;
            }
        }
        if got_pixels {
            if let Some(w) = &self.window {
                w.request_redraw();
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
    let mut exit_after_secs: Option<f64> = None;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
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

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

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
    let reader_proxy = proxy.clone();
    let debug_reader = debug_input;
    thread::spawn(move || {
        let mut reader = reader;
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
        if clip_wanted {
            let _ = tx_reader.send(clipboard_fetch());
        }
        loop {
            let read_started = Instant::now();
            let upd = reader.read_update();
            let read_ms = read_started.elapsed().as_secs_f64() * 1000.0;
            match upd {
                Ok(update) => {
                    for event in update.filecopy.iter() {
                        let _ = tx_fc_reader.send(event.clone());
                    }
                    if clip_wanted {
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
                                    set_local_clipboard_image(&img.uti, &img.data);
                                    continue;
                                }
                                Ok(None) => {}
                                Err(e) => eprintln!("clipboard image decode: {e:#}"),
                            }
                            match pasteboard_text(&archive) {
                                Ok(Some(text)) => {
                                    let echo = clip_echo_reader
                                        .lock()
                                        .map(|e| e.as_deref() == Some(text.as_str()))
                                        .unwrap_or(false);
                                    if debug_reader {
                                        eprintln!(
                                            "clipboard <- {:?}{}",
                                            text,
                                            if echo { " (echo, skipped)" } else { "" }
                                        );
                                    }
                                    if !echo {
                                        if let Ok(mut e) = clip_echo_reader.lock() {
                                            *e = Some(text.clone());
                                        }
                                        set_local_clipboard(&text);
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
                    if tx_upd.send(update).is_err() {
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
        mvs,
        demo_cursor,
        file_dest.clone(),
    )?;
    event_loop.run_app(&mut app)?;
    Ok(())
}
