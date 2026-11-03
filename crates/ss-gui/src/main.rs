//! Live Apple Screen Sharing client window.
//!
//! Runs on macOS for lab testing and on Linux/Wayland via winit + softbuffer.
//! The protocol crate is platform-neutral; this file only translates window
//! events into RFB/Apple messages and presents BGRX frames.

use anyhow::Result;
use gesture_input::{Gesture, Phase};
use rfb_proto::{
    framebuffer_update_request, gesture_begin_end, gesture_magnify,
    gesture_mouse_scroll, gesture_rotate, gesture_scroll, gesture_smart_magnify, gesture_swipe,
    key_event, pointer_event, AppleSession, Update,
};
use std::fs::File;
use std::io::BufWriter;
use std::num::NonZeroU32;
use std::sync::mpsc::{self, Receiver, Sender};
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

fn write_png(path: &str, width: u16, height: u16, bgrx: &[u8]) -> Result<()> {
    let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
    for px in bgrx.chunks_exact(4) {
        rgb.push(px[2]);
        rgb.push(px[1]);
        rgb.push(px[0]);
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
        Gesture::Scroll { dx, dy, phase } => {
            out.push(gesture_scroll(
                dx * 2000.0,
                dy * 2000.0,
                0.0,
                1,
                phase.code(),
                0,
                fb_w / 2,
                fb_h / 2,
            ));
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
    fb: Vec<u8>,
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
            fb: vec![0u8; fb_w as usize * fb_h as usize * 4],
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

    fn send_swipe(&self, motion: u32, mask: u32) {
        let (x, y) = self.cursor;
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
                self.fb = vec![0u8; r.w as usize * r.h as usize * 4];
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
                let dst = (dy * dst_w + x) * 4;
                self.fb[dst..dst + src_row].copy_from_slice(&r.pixels[src..src + src_row]);
            }
            self.dirty = true;
        }
    }

    fn draw(&mut self) -> Result<()> {
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
            // 1:1 fast path.
            for (dst, src) in buffer.iter_mut().zip(self.fb.chunks_exact(4)) {
                *dst = u32::from_le_bytes([src[0], src[1], src[2], 0]);
            }
        } else {
            let fw = self.fb_w as usize;
            let fh = self.fb_h as u64;
            for y in 0..h as usize {
                let sy = (y as u64 * fh / h as u64) as usize;
                let row = sy * fw;
                let out = &mut buffer[y * w as usize..(y + 1) * w as usize];
                for (x, dst) in out.iter_mut().enumerate() {
                    let off = (row + self.xmap[x]) * 4;
                    *dst = u32::from_le_bytes([
                        self.fb[off],
                        self.fb[off + 1],
                        self.fb[off + 2],
                        0,
                    ]);
                }
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
                self.pending_pointer = Some((x, y, self.buttons));
                self.flush_pointer(true);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x * 10.0, y * 10.0),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32, p.y as f32),
                };
                let (x, y) = self.cursor;
                self.send(gesture_mouse_scroll(
                    0,
                    (-dy) as i16,
                    0,
                    (0, 0, 0),
                    (0, (-dy) as i32, 0),
                    2,
                    0,
                    x,
                    y,
                ));
                let _ = dx;
            }
            WindowEvent::PinchGesture { delta, .. } => {
                let (x, y) = self.cursor;
                self.send(gesture_magnify(delta, 2, 0, x, y));
            }
            WindowEvent::RotationGesture { delta, .. } => {
                let (x, y) = self.cursor;
                self.send(gesture_rotate(delta as f64, 2, 0, x, y));
            }
            WindowEvent::DoubleTapGesture { .. } => {
                let (x, y) = self.cursor;
                self.send(gesture_smart_magnify(x, y));
            }
            WindowEvent::PanGesture { delta, phase, .. } => {
                let (x, y) = self.cursor;
                self.send(gesture_scroll(
                    delta.x,
                    delta.y,
                    0.0,
                    1,
                    touch_phase(phase),
                    0,
                    x,
                    y,
                ));
            }
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
            }
            WindowEvent::KeyboardInput { event, .. } => {
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
    let (reader, writer) = session.split()?;
    let req_w = reader.info.width;
    let req_h = reader.info.height;
    let fb_w = ((req_w as f64 * scale).round() as u16).max(1);
    let fb_h = ((req_h as f64 * scale).round() as u16).max(1);
    let input_w = req_w;
    let input_h = req_h;

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

    thread::spawn(move || {
        let mut writer = writer;
        while let Ok(body) = rx_out.recv() {
            if let Err(e) = writer.send(&body) {
                eprintln!("write error: {e:#}");
                break;
            }
        }
    });

    let tx_reader = tx_out.clone();
    let reader_proxy = proxy.clone();
    let debug_reader = debug_input;
    thread::spawn(move || {
        let mut reader = reader;
        let _ = tx_reader.send(framebuffer_update_request(fb_w, fb_h, false));
        loop {
            match reader.read_update() {
                Ok(update) => {
                    if debug_reader {
                        let desc: Vec<_> = update
                            .rects
                            .iter()
                            .map(|r| (r.x, r.y, r.w, r.h, r.encoding, r.pixels.len()))
                            .collect();
                        eprintln!("update rects={desc:?}");
                    }
                    if tx_upd.send(update).is_err() {
                        break;
                    }
                    let _ = reader_proxy.send_event(UserEvent::Refresh);
                    // This server build does not reliably free-run after the
                    // first auto frame. Poll with the *scaled* geometry so
                    // post-input dirty rectangles are always delivered.
                    thread::sleep(Duration::from_millis(16));
                    let _ = tx_reader.send(framebuffer_update_request(fb_w, fb_h, true));
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
