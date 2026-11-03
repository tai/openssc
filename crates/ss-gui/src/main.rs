//! Live Apple Screen Sharing client window.
//!
//! Runs on macOS for lab testing and on Linux/Wayland via winit + softbuffer.
//! The protocol crate is platform-neutral; this file only translates window
//! events into RFB/Apple messages and presents BGRX frames.

use anyhow::Result;
use gesture_input::{Gesture, Phase};
use rfb_proto::{
    client_cut_text, clipboard_fetch, framebuffer_update_request, gesture_begin_end,
    gesture_magnify, gesture_mouse_scroll, gesture_rotate, gesture_smart_magnify, gesture_swipe,
    key_event, pasteboard_text, pointer_event, AppleSession, Update,
};
use std::fs::File;
use std::io::BufWriter;
use std::num::NonZeroU32;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::atomic::{AtomicU64, Ordering};
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

fn note_input() {
    let start = *START.get_or_init(Instant::now);
    LAST_INPUT_MS.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
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

    fn apply_update(&mut self, update: Update) {
        for r in update.rects {
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
            WindowEvent::CursorMoved { position, .. } => {
                note_input();
                let (x, y) = self.to_fb(position);
                if self.debug_input && self.buttons != 0 {
                    eprintln!("cursor fb=({x},{y}) buttons={}", self.buttons);
                }
                if (x, y) != self.cursor {
                    self.cursor = (x, y);
                    self.pending_pointer = Some((x, y, self.buttons));
                    self.flush_pointer(false);
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
                    match write_png(&path, self.fb_w, self.fb_h, &self.fb) {
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
                match write_png(&path, self.fb_w, self.fb_h, &self.fb) {
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
    // Off by default until the fetch round-trip is proven stable; the 0x0b
    // ClipboardFetch makes this server build close the viewer right after it
    // answers (observed "going to close 5").
    let mut clipboard = false;
    // Writing the clipboard needs AutoPasteboard, which puts the server into
    // promise mode and stops it sending real remote clipboard data.
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
    if let Some(enc) = encoding {
        session.send_message(&rfb_proto::set_encodings(&[enc]))?;
        eprintln!("advertised encoding {enc} only");
    }
    if clipboard_send {
        session.send_message(&rfb_proto::auto_pasteboard(1))?;
        eprintln!("clipboard write mode: AutoPasteboard enabled (remote reads become promises)");
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
    if auto_updates {
        session.send_message(&rfb_proto::auto_framebuffer_update(fb_w, fb_h))?;
    }
    session.send_message(&framebuffer_update_request(fb_w, fb_h, false))?;
    for _ in 1..pipeline {
        session.send_message(&framebuffer_update_request(fb_w, fb_h, true))?;
    }
    let (reader, writer) = session.split()?;

    let (tx_out, rx_out) = mpsc::channel::<Vec<u8>>();
    // Bounded: if the GUI cannot keep up, apply backpressure instead of
    // queueing stale frames (which makes interaction feel sluggish).
    let (tx_upd, rx_upd) = mpsc::channel::<Update>();

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
    if clipboard && clipboard_send {
        let tx_clip = tx_out.clone();
        let debug_clip = debug_input;
        let clip_echo = clip_echo.clone();
        thread::spawn(move || {
            let mut last = String::new();
            loop {
                thread::sleep(Duration::from_millis(700));
                let Some(text) = local_clipboard_text() else { continue };
                if text.is_empty() || text == last {
                    continue;
                }
                if clip_echo.lock().map(|e| e.as_deref() == Some(text.as_str())).unwrap_or(false) {
                    // We just wrote this into the local clipboard; do not send
                    // it straight back to the server.
                    last = text;
                    continue;
                }
                last = text.clone();
                if debug_clip {
                    eprintln!("clipboard -> {:?}", text);
                }
                if tx_clip.send(client_cut_text(text.as_bytes())).is_err() {
                    break;
                }
            }
        });
    }

    let clip_echo_reader = clip_echo.clone();
    let tx_reader = tx_out.clone();
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
        let clip_wanted = clipboard && !clipboard_send;
        if clip_wanted {
            let _ = tx_reader.send(clipboard_fetch());
        }
        loop {
            let read_started = Instant::now();
            let upd = reader.read_update();
            let read_ms = read_started.elapsed().as_secs_f64() * 1000.0;
            match upd {
                Ok(update) => {
                    if clip_wanted {
                        if update.clipboard_changed {
                            let _ = tx_reader.send(clipboard_fetch());
                        }
                        for send in &update.clipboard {
                            match send.archive().and_then(|a| pasteboard_text(&a)) {
                                Ok(Some(text)) => {
                                    if debug_reader {
                                        eprintln!("clipboard <- {:?}", text);
                                    }
                                    if let Ok(mut e) = clip_echo_reader.lock() {
                                        *e = Some(text.clone());
                                    }
                                    set_local_clipboard(&text);
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
                    if tx_upd.send(update).is_err() {
                        break;
                    }
                    let _ = reader_proxy.send_event(UserEvent::Refresh);
                    // This server build does not reliably free-run after the
                    // first auto frame. Poll with the *scaled* geometry so
                    // post-input dirty rectangles are always delivered.
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
                    if bands > 1 && input_is_hot() {
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
    )?;
    event_loop.run_app(&mut app)?;
    Ok(())
}
