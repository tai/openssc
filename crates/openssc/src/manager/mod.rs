//! The connection-manager window.
//!
//! A deliberately small custom UI on the same winit 0.30 + softbuffer stack as
//! the session window. There is no GUI toolkit and no text-shaping stack: the
//! labels and fields use the embedded 5x7 bitmap font in [`font`], and every
//! control is a rectangle. The manager never grabs the pointer or keyboard and
//! never opens an RFB connection itself — Connect spawns a second copy of this
//! binary in direct-connect mode and stays open for the next target.
//!
//! The interesting, testable half (saved targets and state transitions) lives
//! in [`targets`] and [`state`]; this file is the window, layout and input
//! mapping around them.

mod font;
mod state;
mod targets;

use std::num::NonZeroU32;
use std::sync::Arc;

use anyhow::Result;
use raw_window_handle::{DisplayHandle, HasDisplayHandle};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use state::{Button, Field, Focus, ManagerState};
use targets::{Target, TargetBook};

// A calm dark palette. Values are 0x00RRGGBB, which softbuffer's XRGB8888
// little-endian buffer stores as B,G,R,X bytes.
const BG: u32 = 0x00171a1f;
const PANEL: u32 = 0x0020252b;
const PANEL_ALT: u32 = 0x00272e36;
const FIELD: u32 = 0x002c333c;
const BORDER: u32 = 0x003b4552;
const TEXT: u32 = 0x00e8edf2;
const MUTED: u32 = 0x0094a0ad;
const ACCENT: u32 = 0x006ea8ff;
const ACCENT_DARK: u32 = 0x003b6fd6;
const SELECT: u32 = 0x002b3a52;
const DANGER: u32 = 0x00e06c75;
const OK: u32 = 0x0098c379;

/// Open the manager. The caller has already parsed `--manager`, so nothing
/// here touches the session CLI or connects before the user asks.
pub fn run() -> Result<()> {
    let path = targets::default_config_path();
    let book = match targets::load(&path) {
        Ok(book) => book,
        Err(error) => {
            eprintln!("manager: cannot read {}: {error}", path.display());
            eprintln!("manager: starting with the built-in seed; Save will retry");
            TargetBook {
                targets: vec![Target::seed()],
                warnings: vec![format!("cannot read {}: {error}", path.display())],
                path: Some(path.clone()),
            }
        }
    };
    eprintln!(
        "manager: {} target(s) from {} (passwords are never stored)",
        book.targets.len(),
        path.display()
    );
    let state = ManagerState::new(book);

    let event_loop = EventLoop::<()>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = ManagerApp::new(&event_loop, state)?;
    event_loop.run_app(&mut app)?;
    Ok(())
}

struct ManagerApp {
    /// `Option` so `exiting` can drop the softbuffer context while winit still
    /// owns the Wayland display, the same lifetime dance as the session App.
    context: Option<softbuffer::Context<DisplayHandle<'static>>>,
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<DisplayHandle<'static>, Arc<Window>>>,
    surface_size: (u32, u32),
    state: ManagerState,
    modifiers: ModifiersState,
    cursor: (f64, f64),
    title: String,
}

impl ManagerApp {
    fn new(event_loop: &EventLoop<()>, state: ManagerState) -> Result<Self> {
        let display = event_loop.display_handle()?;
        // SAFETY: `exiting` clears the context before `run_app` returns; the
        // display handle is valid until then.
        let display =
            unsafe { std::mem::transmute::<DisplayHandle<'_>, DisplayHandle<'static>>(display) };
        let context = softbuffer::Context::new(display)
            .map_err(|e| anyhow::anyhow!("softbuffer context: {e:?}"))?;
        Ok(Self {
            context: Some(context),
            window: None,
            surface: None,
            surface_size: (0, 0),
            state,
            modifiers: ModifiersState::empty(),
            cursor: (0.0, 0.0),
            title: String::new(),
        })
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
        let scale = window.scale_factor().round().clamp(1.0, 3.0) as i32;
        let layout = Layout::compute(w as i32, h as i32, scale);

        let Some(surface) = self.surface.as_mut() else {
            return Ok(());
        };
        if self.surface_size != (w, h) {
            surface
                .resize(
                    NonZeroU32::new(w).expect("non-zero width"),
                    NonZeroU32::new(h).expect("non-zero height"),
                )
                .map_err(|e| anyhow::anyhow!("softbuffer resize: {e:?}"))?;
            self.surface_size = (w, h);
        }
        let mut buffer = surface
            .buffer_mut()
            .map_err(|e| anyhow::anyhow!("softbuffer buffer: {e:?}"))?;
        {
            let mut canvas = Canvas {
                buf: &mut buffer[..],
                w: w as i32,
                h: h as i32,
            };
            Self::paint(&self.state, &mut canvas, &layout, scale);
        }
        let title = if self.state.is_dirty() {
            "OpenSSC — Connections *"
        } else {
            "OpenSSC — Connections"
        };
        if title != self.title {
            window.set_title(title);
            self.title = title.to_string();
        }
        window.pre_present_notify();
        buffer
            .present()
            .map_err(|e| anyhow::anyhow!("softbuffer present: {e:?}"))?;
        Ok(())
    }

    fn paint(state: &ManagerState, canvas: &mut Canvas<'_>, layout: &Layout, s: i32) {
        canvas.fill(Rect::new(0, 0, canvas.w, canvas.h), BG);
        paint_header(canvas, layout, s, &state);

        // Saved-target list.
        canvas.fill(layout.list, PANEL);
        canvas.stroke(layout.list, BORDER, s);
        canvas.text(
            layout.list.x + 10 * s,
            layout.list.y + 8 * s,
            2 * s,
            MUTED,
            "SAVED TARGETS",
        );
        let rows = layout.visible_rows();
        let offset = list_offset(state.selected, state.targets.len(), rows);
        if state.targets.is_empty() {
            canvas.text(
                layout.list.x + 10 * s,
                layout.rows_y(),
                2 * s,
                MUTED,
                "No targets — press Add",
            );
        }
        for visible in 0..rows {
            let index = offset + visible;
            let Some(target) = state.targets.get(index) else {
                break;
            };
            let row = layout.row_rect(visible);
            let selected = state.selected == Some(index);
            if selected {
                canvas.fill(row, SELECT);
                canvas.fill(Rect::new(row.x, row.y, 3 * s, row.h), ACCENT);
            }
            if selected && state.focus == Focus::List {
                canvas.stroke(row, ACCENT, s);
            }
            let pad = 10 * s;
            let max = row.w - 2 * pad;
            // All four saved fields stay visible even when the compositor
            // tiles the window narrow: name and port share the first line,
            // user@host the second. Long values are elided, not cut mid-pixel.
            let port = format!(":{}", target.port);
            let port_w = canvas.text_width(&port, 2 * s);
            canvas.text_elided(
                row.x + pad,
                row.y + 7 * s,
                2 * s,
                TEXT,
                &target.name,
                (max - port_w - 8 * s).max(0),
            );
            canvas.text_elided(
                row.x + row.w - pad - port_w,
                row.y + 7 * s,
                2 * s,
                MUTED,
                &port,
                port_w,
            );
            let detail = format!("{}@{}", target.user, target.host);
            canvas.text_elided(row.x + pad, row.y + 28 * s, 2 * s, MUTED, &detail, max);
        }

        // Form.
        let max_text = layout.form_w - 20 * s;
        for (index, field) in Field::ALL.iter().enumerate() {
            let rect = layout.fields[index];
            canvas.text(rect.x, rect.y - 17 * s, 2 * s, MUTED, field.label());
            let focused = state.focus == Focus::Field(*field);
            canvas.fill(rect, FIELD);
            canvas.stroke(
                rect,
                if focused { ACCENT } else { BORDER },
                if focused { 2 * s } else { s },
            );
            let value = if *field == Field::Password {
                state.form.masked_password()
            } else {
                state.form.field(*field).to_string()
            };
            let text_x = rect.x + 10 * s;
            let text_y = rect.y + (rect.h - 14 * s) / 2;
            if value.is_empty() {
                let hint = if *field == Field::Password {
                    "not stored"
                } else {
                    ""
                };
                if !hint.is_empty() {
                    canvas.text_clipped(text_x, text_y, 2 * s, MUTED, hint, max_text);
                }
            } else {
                canvas.text_clipped(text_x, text_y, 2 * s, TEXT, &value, max_text);
            }
            if focused {
                let caret_x =
                    (text_x + canvas.text_width(&value, 2 * s) + s).min(rect.x + rect.w - 4 * s);
                canvas.fill(Rect::new(caret_x, text_y - 2 * s, 2 * s, 18 * s), ACCENT);
            }
        }

        // Buttons.
        let has_selection = state.selected.is_some();
        for (index, button) in Button::ALL.iter().enumerate() {
            let rect = layout.buttons[index];
            let focused = state.focus == Focus::Button(*button);
            let enabled = *button != Button::Delete || has_selection;
            let fill = if !enabled {
                PANEL
            } else if button.primary() {
                ACCENT_DARK
            } else {
                PANEL_ALT
            };
            canvas.fill(rect, fill);
            canvas.stroke(
                rect,
                if focused {
                    ACCENT
                } else if button.primary() && enabled {
                    ACCENT_DARK
                } else {
                    BORDER
                },
                if focused { 2 * s } else { s },
            );
            let color = if !enabled {
                MUTED
            } else if button.primary() {
                TEXT
            } else {
                TEXT
            };
            let tw = canvas.text_width(button.label(), 2 * s);
            canvas.text(
                rect.x + (rect.w - tw) / 2,
                rect.y + (rect.h - 14 * s) / 2,
                2 * s,
                color,
                button.label(),
            );
        }

        // Status line.
        canvas.fill(layout.status, BG);
        canvas.fill(
            Rect::new(layout.pad, layout.status.y, canvas.w - 2 * layout.pad, s),
            BORDER,
        );
        let color = if state.status.starts_with("saved") {
            OK
        } else if state.status.contains("failed")
            || state.status.contains("required")
            || state.status.contains("must")
            || state.status.contains("invalid")
        {
            DANGER
        } else {
            MUTED
        };
        canvas.text_clipped(
            layout.pad,
            layout.status.y + (layout.status.h - 14 * s) / 2,
            2 * s,
            color,
            &state.status,
            canvas.w - 2 * layout.pad,
        );
    }

    fn hit(&self, x: f64, y: f64) -> Hit {
        let Some(window) = &self.window else {
            return Hit::None;
        };
        let size = window.inner_size();
        let scale = window.scale_factor().round().clamp(1.0, 3.0) as i32;
        let layout = Layout::compute(size.width as i32, size.height as i32, scale);
        let (x, y) = (x.round() as i32, y.round() as i32);
        let rows = layout.visible_rows();
        let offset = list_offset(self.state.selected, self.state.targets.len(), rows);
        if layout.list.contains(x, y) {
            let local = y - layout.rows_y();
            if local >= 0 {
                let row = (local / layout.row_h) as usize;
                if row < rows {
                    let index = offset + row;
                    if index < self.state.targets.len() {
                        return Hit::List(index);
                    }
                }
            }
        }
        for (index, field) in Field::ALL.iter().enumerate() {
            if layout.fields[index].contains(x, y) {
                return Hit::Field(*field);
            }
        }
        for (index, button) in Button::ALL.iter().enumerate() {
            if layout.buttons[index].contains(x, y) {
                return Hit::Button(*button);
            }
        }
        Hit::None
    }

    fn click(&mut self, x: f64, y: f64) {
        match self.hit(x, y) {
            Hit::List(index) => self.state.select(index),
            Hit::Field(field) => self.state.focus_field(field),
            Hit::Button(button) => {
                self.state.focus_button(button);
                self.press(button);
            }
            Hit::None => return,
        }
        self.redraw();
    }

    fn press(&mut self, button: Button) {
        match button {
            Button::Add => self.state.add(),
            Button::Save => {
                self.state.save();
            }
            Button::Delete => self.state.delete_selected(),
            Button::Connect => self.connect(),
        }
    }

    fn connect(&mut self) {
        match self.state.connect_request() {
            Ok(request) => match request.spawn() {
                Ok(pid) => {
                    self.state.status = format!(
                        "started {:?} (pid {pid}) — manager stays open",
                        request.target.name
                    );
                }
                Err(error) => {
                    self.state.status = format!("launch failed: {error}");
                }
            },
            Err(errors) => self.state.status = errors.join("; "),
        }
    }

    fn activate(&mut self) {
        match self.state.focus {
            Focus::Button(button) => self.press(button),
            _ => self.connect(),
        }
    }

    fn redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn key(&mut self, event_loop: &ActiveEventLoop, event: &winit::event::KeyEvent) {
        match &event.logical_key {
            Key::Named(NamedKey::Tab) => {
                self.state.tab(self.modifiers.shift_key());
            }
            Key::Named(NamedKey::Enter) => self.activate(),
            Key::Named(NamedKey::Escape) => {
                event_loop.exit();
                return;
            }
            Key::Named(NamedKey::Backspace) => self.state.backspace(),
            Key::Named(NamedKey::Delete) => self.state.clear_field(),
            Key::Named(NamedKey::ArrowUp) if self.state.focus == Focus::List => {
                self.state.select_delta(-1)
            }
            Key::Named(NamedKey::ArrowDown) if self.state.focus == Focus::List => {
                self.state.select_delta(1)
            }
            Key::Named(NamedKey::ArrowLeft) => self.state.tab(true),
            Key::Named(NamedKey::ArrowRight) => self.state.tab(false),
            Key::Named(NamedKey::Home) if self.state.focus == Focus::List => self.state.select(0),
            Key::Named(NamedKey::End) if self.state.focus == Focus::List => {
                if let Some(last) = self.state.targets.len().checked_sub(1) {
                    self.state.select(last);
                }
            }
            Key::Named(NamedKey::Space) => self.state.edit_char(' '),
            _ => {
                if self.modifiers.control_key()
                    || self.modifiers.super_key()
                    || self.modifiers.alt_key()
                {
                    return;
                }
                if let Some(text) = &event.text {
                    for c in text.chars() {
                        self.state.edit_char(c);
                    }
                }
            }
        }
        self.redraw();
    }
}

impl ApplicationHandler<()> for ManagerApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("OpenSSC — Connections")
            .with_inner_size(LogicalSize::new(900.0, 560.0))
            .with_min_inner_size(LogicalSize::new(720.0, 470.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        let Some(context) = self.context.as_ref() else {
            return;
        };
        let surface = softbuffer::Surface::new(context, window.clone())
            .unwrap_or_else(|e| panic!("softbuffer surface: {e:?}"));
        self.surface = Some(surface);
        self.window = Some(window);
        if let Some(window) = &self.window {
            window.set_visible(true);
            window.focus_window();
            window.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(_) => self.redraw(),
            WindowEvent::RedrawRequested => {
                if let Err(error) = self.draw() {
                    eprintln!("manager: draw error: {error:#}");
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x, position.y);
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                let (x, y) = self.cursor;
                self.click(x, y);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let steps = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(position) => position.y as f32 / 30.0,
                };
                if steps.abs() < 0.1 {
                    return;
                }
                // Only scroll when the pointer is over the list, so a stray
                // wheel event over the form cannot change the selection.
                if !matches!(self.hit(self.cursor.0, self.cursor.1), Hit::List(_)) {
                    return;
                }
                self.state.select_delta(if steps < 0.0 { 1 } else { -1 });
                self.redraw();
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed && !event.repeat =>
            {
                self.key(event_loop, &event);
            }
            _ => {}
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // Drop in this order: the surface borrows the context, and the context
        // borrows winit's Wayland display, which `run_app` still owns here.
        self.surface = None;
        self.window = None;
        self.context = None;
    }
}

#[derive(Clone, Copy, Debug)]
enum Hit {
    None,
    List(usize),
    Field(Field),
    Button(Button),
}

fn list_offset(selected: Option<usize>, len: usize, rows: usize) -> usize {
    if rows == 0 {
        return 0;
    }
    match selected {
        Some(index) if index >= rows => (index + 1 - rows).min(len.saturating_sub(rows)),
        _ => 0,
    }
}

fn paint_header(canvas: &mut Canvas<'_>, layout: &Layout, s: i32, state: &ManagerState) {
    canvas.fill(
        Rect::new(layout.pad, layout.pad + 4 * s, 4 * s, 24 * s),
        ACCENT,
    );
    canvas.text(
        layout.pad + 12 * s,
        layout.pad + 5 * s,
        3 * s,
        TEXT,
        "OpenSSC",
    );
    let title_w = canvas.text_width("OpenSSC", 3 * s);
    canvas.text(
        layout.pad + 12 * s + title_w + 10 * s,
        layout.pad + 5 * s,
        3 * s,
        MUTED,
        "CONNECTIONS",
    );
    if state.is_dirty() {
        let connections_w = canvas.text_width("CONNECTIONS", 3 * s);
        canvas.text(
            layout.pad + 22 * s + title_w + connections_w + 6 * s,
            layout.pad + 5 * s,
            3 * s,
            ACCENT,
            "*",
        );
    }
    let hint = "Tab move   Enter connect   Esc close";
    let hint_w = canvas.text_width(hint, 2 * s);
    if layout.w - layout.pad - hint_w > layout.pad + 12 * s + title_w + 10 * s {
        canvas.text(
            layout.w - layout.pad - hint_w,
            layout.pad + 12 * s,
            2 * s,
            MUTED,
            hint,
        );
    }
}

#[derive(Clone, Copy, Debug)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect {
    fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

struct Layout {
    w: i32,
    pad: i32,
    list: Rect,
    fields: [Rect; 5],
    buttons: [Rect; 4],
    status: Rect,
    row_h: i32,
    rows_y: i32,
    form_w: i32,
}

impl Layout {
    fn compute(w: i32, h: i32, s: i32) -> Self {
        let pad = 14 * s;
        let header_h = 48 * s;
        let status_h = 30 * s;
        // The compositor owns the actual size: Hyprland may tile this much
        // narrower than the requested 900x560, so the list and form shrink and
        // the buttons wrap rather than running off the surface.
        // A tiled window gets a wider list so name, port and user@host all
        // remain readable; at the requested size the form gets the space.
        let list_pct = if w < 620 * s { 47 } else { 34 };
        let list_w = (w * list_pct / 100).clamp(110 * s, 360 * s);
        let content_top = pad + header_h;
        let list = Rect::new(
            pad,
            content_top,
            list_w,
            (h - content_top - status_h - pad).max(80 * s),
        );
        let form_x = pad + list_w + pad;
        let form_w = (w - form_x - pad).max(120 * s);
        let gap = 10 * s;
        let button_h = 34 * s;
        let one_row = form_w >= 380 * s;
        let button_rows = if one_row { 1 } else { 2 };
        let buttons_total = button_rows * button_h + (button_rows - 1) * gap;
        let bottom = h - status_h - pad;
        let field_space = (bottom - content_top - 12 * s - buttons_total).max(5 * 38 * s);
        let field_h = (field_space / 5).clamp(38 * s, 56 * s);
        let box_h = (field_h - 20 * s).clamp(20 * s, 32 * s);
        let mut fields = [Rect::new(0, 0, 0, 0); 5];
        for (index, rect) in fields.iter_mut().enumerate() {
            *rect = Rect::new(
                form_x,
                content_top + (index as i32) * field_h + 18 * s,
                form_w,
                box_h,
            );
        }
        let button_y = content_top + 5 * field_h + 12 * s;
        let buttons = if one_row {
            let available = form_w - 3 * gap;
            let connect_w = (available * 40 / 100).clamp(90 * s, 160 * s);
            let small_w = ((available - connect_w) / 3).max(50 * s);
            [
                Rect::new(form_x, button_y, small_w, button_h),
                Rect::new(form_x + small_w + gap, button_y, small_w, button_h),
                Rect::new(form_x + 2 * (small_w + gap), button_y, small_w, button_h),
                Rect::new(form_x + form_w - connect_w, button_y, connect_w, button_h),
            ]
        } else {
            let half = ((form_w - gap) / 2).max(60 * s);
            let y2 = button_y + button_h + gap;
            [
                Rect::new(form_x, button_y, half, button_h),
                Rect::new(form_x + half + gap, button_y, half, button_h),
                Rect::new(form_x, y2, half, button_h),
                Rect::new(form_x + half + gap, y2, half, button_h),
            ]
        };
        let status = Rect::new(pad, h - status_h, w - 2 * pad, status_h);
        Self {
            w,
            pad,
            list,
            fields,
            buttons,
            status,
            row_h: 54 * s,
            rows_y: list.y + 26 * s,
            form_w,
        }
    }

    fn rows_y(&self) -> i32 {
        self.rows_y
    }

    fn visible_rows(&self) -> usize {
        // rows_y() is expressed in pixels above the first row; the row height
        // already carries the scale factor.
        ((self.list.y + self.list.h - self.rows_y() - 6) / self.row_h).max(0) as usize
    }

    fn row_rect(&self, index: usize) -> Rect {
        Rect::new(
            self.list.x + 1,
            self.rows_y() + index as i32 * self.row_h,
            self.list.w - 2,
            self.row_h,
        )
    }
}

struct Canvas<'a> {
    buf: &'a mut [u32],
    w: i32,
    h: i32,
}

impl Canvas<'_> {
    fn fill(&mut self, rect: Rect, color: u32) {
        let x0 = rect.x.max(0);
        let y0 = rect.y.max(0);
        let x1 = (rect.x + rect.w).min(self.w);
        let y1 = (rect.y + rect.h).min(self.h);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        for y in y0..y1 {
            let start = (y * self.w + x0) as usize;
            let end = (y * self.w + x1) as usize;
            self.buf[start..end].fill(color);
        }
    }

    fn stroke(&mut self, rect: Rect, color: u32, thickness: i32) {
        let t = thickness.max(1);
        self.fill(Rect::new(rect.x, rect.y, rect.w, t), color);
        self.fill(Rect::new(rect.x, rect.y + rect.h - t, rect.w, t), color);
        self.fill(Rect::new(rect.x, rect.y, t, rect.h), color);
        self.fill(Rect::new(rect.x + rect.w - t, rect.y, t, rect.h), color);
    }

    fn text_width(&self, text: &str, scale: i32) -> i32 {
        text.chars().count() as i32 * font::CELL_W as i32 * scale
    }

    fn text(&mut self, x: i32, y: i32, scale: i32, color: u32, text: &str) {
        self.text_clipped(x, y, scale, color, text, i32::MAX);
    }

    /// Like [`Self::text_clipped`], but marks a value that did not fit with
    /// an ellipsis instead of slicing the last glyph in half.
    fn text_elided(&mut self, x: i32, y: i32, scale: i32, color: u32, text: &str, max_width: i32) {
        let cell = (font::CELL_W as i32 * scale).max(1);
        let fit = (max_width / cell).max(0) as usize;
        let count = text.chars().count();
        if count <= fit {
            self.text(x, y, scale, color, text);
        } else if fit >= 4 {
            let mut elided: String = text.chars().take(fit - 3).collect();
            elided.push_str("...");
            self.text(x, y, scale, color, &elided);
        } else {
            self.text_clipped(x, y, scale, color, text, max_width);
        }
    }

    fn text_clipped(&mut self, x: i32, y: i32, scale: i32, color: u32, text: &str, max_width: i32) {
        if scale <= 0 || max_width <= 0 {
            return;
        }
        let mut pen = x;
        for c in text.chars() {
            if pen + font::CELL_W as i32 * scale > x.saturating_add(max_width) {
                break;
            }
            if let Some(glyph) = font::glyph(c) {
                for (col, bits) in glyph.iter().enumerate() {
                    for row in 0..font::CELL_H {
                        if bits & (1 << row) != 0 {
                            self.fill(
                                Rect::new(
                                    pen + col as i32 * scale,
                                    y + row as i32 * scale,
                                    scale,
                                    scale,
                                ),
                                color,
                            );
                        }
                    }
                }
            }
            pen += font::CELL_W as i32 * scale;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_keeps_the_form_and_buttons_inside_the_window() {
        for (w, h, s) in [
            (900, 560, 1),
            (1800, 1120, 2),
            (720, 470, 1),
            (952, 896, 2),
            (656, 890, 2),
        ] {
            let layout = Layout::compute(w, h, s);
            assert!(layout.list.x >= 0 && layout.list.y >= 0);
            assert!(layout.list.x + layout.list.w < layout.fields[0].x);
            for rect in layout.fields.iter().chain(layout.buttons.iter()) {
                assert!(rect.x >= 0 && rect.y >= 0);
                assert!(rect.x + rect.w <= w, "{rect:?} exceeds {w}");
                assert!(rect.y + rect.h <= h, "{rect:?} exceeds {h}");
            }
            assert!(layout.status.y + layout.status.h <= h);
            assert!(layout.visible_rows() > 0);
        }
    }

    #[test]
    fn list_rows_scroll_to_keep_the_selection_visible() {
        assert_eq!(list_offset(Some(0), 10, 5), 0);
        assert_eq!(list_offset(Some(4), 10, 5), 0);
        assert_eq!(list_offset(Some(5), 10, 5), 1);
        assert_eq!(list_offset(Some(9), 10, 5), 5);
        assert_eq!(list_offset(None, 10, 5), 0);
    }

    #[test]
    fn font_covers_printable_ascii_and_skips_the_rest() {
        for code in 32..=126u32 {
            assert!(font::glyph(char::from_u32(code).unwrap()).is_some());
        }
        assert!(font::glyph('\u{00e9}').is_none());
        assert!(font::glyph('\n').is_none());
    }

    #[test]
    fn canvas_clips_text_and_rectangles_at_the_edges() {
        let mut pixels = vec![0u32; 20 * 10];
        {
            let mut canvas = Canvas {
                buf: &mut pixels,
                w: 20,
                h: 10,
            };
            canvas.fill(Rect::new(-5, -5, 10, 10), 0x00ff0000);
            canvas.text_clipped(0, 0, 1, 0x00ffffff, "hello", 30);
            canvas.stroke(Rect::new(18, 8, 5, 5), 0x0000ff00, 1);
        }
        assert!(pixels.iter().any(|p| *p == 0x00ff0000));
        assert!(pixels.iter().any(|p| *p == 0x00ffffff));
        assert_eq!(pixels[9 * 20 + 18], 0x0000ff00);
    }
}
