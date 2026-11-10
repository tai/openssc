//! Pure manager state: the target list, the edit form, focus and validation.
//!
//! Nothing here opens a window or touches the network. The window module maps
//! keys and clicks onto these transitions, which keeps the interesting
//! behaviour unit-testable without a compositor.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::targets::{self, Target, TargetBook};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Name,
    Host,
    Port,
    User,
    Password,
}

impl Field {
    pub const ALL: [Field; 5] = [
        Field::Name,
        Field::Host,
        Field::Port,
        Field::User,
        Field::Password,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Field::Name => "Name",
            Field::Host => "Host",
            Field::Port => "Port",
            Field::User => "User",
            Field::Password => "Password",
        }
    }

    fn next(self) -> Field {
        match self {
            Field::Name => Field::Host,
            Field::Host => Field::Port,
            Field::Port => Field::User,
            Field::User => Field::Password,
            Field::Password => Field::Name,
        }
    }

    fn prev(self) -> Field {
        match self {
            Field::Name => Field::Password,
            Field::Host => Field::Name,
            Field::Port => Field::Host,
            Field::User => Field::Port,
            Field::Password => Field::User,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Add,
    Save,
    Delete,
    Connect,
}

impl Button {
    pub const ALL: [Button; 4] = [Button::Add, Button::Save, Button::Delete, Button::Connect];

    pub fn label(self) -> &'static str {
        match self {
            Button::Add => "Add",
            Button::Save => "Save",
            Button::Delete => "Delete",
            Button::Connect => "Connect",
        }
    }

    pub fn primary(self) -> bool {
        self == Button::Connect
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    List,
    Field(Field),
    Button(Button),
}

impl Focus {
    fn next(self) -> Focus {
        match self {
            Focus::List => Focus::Field(Field::Name),
            Focus::Field(field) => match field {
                Field::Password => Focus::Button(Button::Add),
                other => Focus::Field(other.next()),
            },
            Focus::Button(button) => match button {
                Button::Connect => Focus::List,
                other => {
                    let index = Button::ALL.iter().position(|b| *b == other).unwrap();
                    Focus::Button(Button::ALL[index + 1])
                }
            },
        }
    }

    fn prev(self) -> Focus {
        match self {
            Focus::List => Focus::Button(Button::Connect),
            Focus::Field(field) => match field {
                Field::Name => Focus::List,
                other => Focus::Field(other.prev()),
            },
            Focus::Button(button) => match button {
                Button::Add => Focus::Field(Field::Password),
                other => {
                    let index = Button::ALL.iter().position(|b| *b == other).unwrap();
                    Focus::Button(Button::ALL[index - 1])
                }
            },
        }
    }
}

/// Editable copy of a selected target plus the one-shot password.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Form {
    pub name: String,
    pub host: String,
    pub port: String,
    pub user: String,
    pub password: String,
}

impl Form {
    pub fn from_target(target: &Target) -> Self {
        Self {
            name: target.name.clone(),
            host: target.host.clone(),
            port: target.port.to_string(),
            user: target.user.clone(),
            password: String::new(),
        }
    }

    pub fn field(&self, field: Field) -> &str {
        match field {
            Field::Name => &self.name,
            Field::Host => &self.host,
            Field::Port => &self.port,
            Field::User => &self.user,
            Field::Password => &self.password,
        }
    }

    pub fn field_mut(&mut self, field: Field) -> &mut String {
        match field {
            Field::Name => &mut self.name,
            Field::Host => &mut self.host,
            Field::Port => &mut self.port,
            Field::User => &mut self.user,
            Field::Password => &mut self.password,
        }
    }

    /// The password shown in the field: one `*` per character, never the text.
    pub fn masked_password(&self) -> String {
        "*".repeat(self.password.chars().count())
    }

    /// Build a target from the form. The password is deliberately left out.
    pub fn to_target(&self) -> Result<Target, Vec<String>> {
        let mut errors = Vec::new();
        let port_text = self.port.trim();
        let port = match port_text.parse::<u16>() {
            Ok(port) => port,
            Err(_) => {
                errors.push("port must be a number between 1 and 65535".to_string());
                0
            }
        };
        let target = Target {
            name: self.name.trim().to_string(),
            host: self.host.trim().to_string(),
            port,
            user: self.user.trim().to_string(),
        };
        let mut target_errors = target.validate();
        if !port_text.is_empty() && port_text.parse::<u16>().is_err() {
            // The parse error above is more precise than "port must be ...".
            target_errors.retain(|error| !error.starts_with("port must"));
        }
        errors.extend(target_errors);
        if errors.is_empty() {
            Ok(target)
        } else {
            Err(errors)
        }
    }
}

/// Everything the manager window displays. Focus starts on the list so the
/// first arrow key selects a target.
#[derive(Clone, Debug)]
pub struct ManagerState {
    pub targets: Vec<Target>,
    pub selected: Option<usize>,
    pub focus: Focus,
    pub form: Form,
    pub status: String,
    pub path: PathBuf,
    /// Form edits not committed to the in-memory list yet.
    pub dirty: bool,
    /// The in-memory list differs from the file (Add/Delete pending a Save).
    pub list_dirty: bool,
}

impl ManagerState {
    pub fn new(book: TargetBook) -> Self {
        let path = book.path.unwrap_or_else(targets::default_config_path);
        let selected = (!book.targets.is_empty()).then_some(0);
        let form = selected
            .map(|i| Form::from_target(&book.targets[i]))
            .unwrap_or_default();
        let status = if let Some(warning) = book.warnings.first() {
            format!("loaded with {} warning(s): {warning}", book.warnings.len())
        } else if book.targets.is_empty() {
            "no targets yet — press Add".to_string()
        } else {
            format!(
                "{} saved target(s) — Enter or Connect to open one",
                book.targets.len()
            )
        };
        Self {
            targets: book.targets,
            selected,
            focus: Focus::List,
            form,
            status,
            path,
            dirty: false,
            list_dirty: false,
        }
    }

    /// True when closing now would lose something: a form edit or a pending
    /// Add/Delete that has not been written to the target file.
    pub fn is_dirty(&self) -> bool {
        self.dirty || self.list_dirty
    }

    /// Commit the form to the in-memory list when it is valid. Invalid edits
    /// stay in the form and are reported instead of silently replacing a saved
    /// target.
    fn commit_form(&mut self) -> bool {
        if !self.dirty {
            return self.form.to_target().is_ok();
        }
        match self.form.to_target() {
            Ok(target) => {
                match self.selected {
                    Some(index) if index < self.targets.len() => self.targets[index] = target,
                    _ => {
                        self.targets.push(target);
                        self.selected = Some(self.targets.len() - 1);
                    }
                }
                self.dirty = false;
                true
            }
            Err(errors) => {
                self.status = errors.join("; ");
                false
            }
        }
    }

    pub fn select(&mut self, index: usize) {
        if self.targets.is_empty() {
            return;
        }
        let index = index.min(self.targets.len() - 1);
        if self.selected == Some(index) {
            return;
        }
        let discarded = self.dirty && self.form.to_target().is_err();
        if self.dirty {
            self.commit_form();
        }
        self.selected = Some(index);
        self.form = Form::from_target(&self.targets[index]);
        self.dirty = false;
        self.status = if discarded {
            format!(
                "unsaved edits discarded — selected {:?}",
                self.targets[index].name
            )
        } else {
            format!("selected {:?}", self.targets[index].name)
        };
    }

    /// Up/Down in the list. Clamps at both ends rather than wrapping, which
    /// makes a held arrow key predictable.
    pub fn select_delta(&mut self, delta: isize) {
        if self.targets.is_empty() {
            return;
        }
        let current = self.selected.unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, self.targets.len() as isize - 1);
        self.select(next as usize);
    }

    pub fn add(&mut self) {
        if self.dirty {
            self.commit_form();
        }
        self.targets.push(Target::draft());
        self.selected = Some(self.targets.len() - 1);
        self.form = Form::from_target(self.targets.last().expect("just pushed"));
        self.dirty = true;
        self.list_dirty = true;
        self.focus = Focus::Field(Field::Name);
        self.status = "new target — fill in Name and Host, then Save".to_string();
    }

    pub fn delete_selected(&mut self) {
        let Some(index) = self.selected else {
            self.status = "nothing selected".to_string();
            return;
        };
        let removed = self.targets.remove(index);
        self.dirty = false;
        self.list_dirty = true;
        if self.targets.is_empty() {
            self.selected = None;
            self.form = Form::default();
            self.focus = Focus::List;
            self.status = format!("deleted {:?} — no targets left", removed.name);
        } else {
            let next = index.min(self.targets.len() - 1);
            self.selected = Some(next);
            self.form = Form::from_target(&self.targets[next]);
            self.status = format!("deleted {:?} — Save to write the list", removed.name);
        }
    }

    /// Validate every target, then write the file atomically. Returns whether
    /// the file was written.
    pub fn save(&mut self) -> bool {
        let target = match self.form.to_target() {
            Ok(target) => target,
            Err(errors) => {
                self.status = errors.join("; ");
                return false;
            }
        };
        match self.selected {
            Some(index) if index < self.targets.len() => self.targets[index] = target,
            _ => {
                self.targets.push(target);
                self.selected = Some(self.targets.len() - 1);
            }
        }
        for (index, target) in self.targets.iter().enumerate() {
            let errors = target.validate();
            if !errors.is_empty() {
                self.selected = Some(index);
                self.form = Form::from_target(target);
                self.dirty = false;
                self.status = format!(
                    "target {} ({:?}): {}",
                    index + 1,
                    target.name,
                    errors.join("; ")
                );
                return false;
            }
        }
        match targets::save(&self.path, &self.targets) {
            Ok(()) => {
                self.dirty = false;
                self.list_dirty = false;
                self.status = format!(
                    "saved {} target(s) to {}",
                    self.targets.len(),
                    self.path.display()
                );
                true
            }
            Err(error) => {
                self.status = format!("save failed: {error}");
                false
            }
        }
    }

    /// Build a launch request from the form. Validation happens before any
    /// process is spawned, so a typo never becomes a failed child.
    pub fn connect_request(&self) -> Result<LaunchRequest, Vec<String>> {
        let target = self.form.to_target()?;
        Ok(LaunchRequest {
            target,
            password: self.form.password.clone(),
        })
    }

    pub fn edit_char(&mut self, c: char) {
        let Focus::Field(field) = self.focus else {
            return;
        };
        if c.is_control() {
            return;
        }
        if field == Field::Port && !c.is_ascii_digit() {
            return;
        }
        let limit = match field {
            Field::Name => 64,
            Field::Host => 253,
            Field::Port => 5,
            Field::User => 64,
            Field::Password => 128,
        };
        let value = self.form.field_mut(field);
        if value.chars().count() < limit {
            value.push(c);
            self.dirty = true;
            self.status.clear();
        }
    }

    pub fn backspace(&mut self) {
        if let Focus::Field(field) = self.focus {
            let value = self.form.field_mut(field);
            if value.pop().is_some() {
                self.dirty = true;
                self.status.clear();
            }
        }
    }

    pub fn clear_field(&mut self) {
        if let Focus::Field(field) = self.focus {
            let value = self.form.field_mut(field);
            if !value.is_empty() {
                value.clear();
                self.dirty = true;
                self.status.clear();
            }
        }
    }

    pub fn tab(&mut self, backwards: bool) {
        self.focus = if backwards {
            self.focus.prev()
        } else {
            self.focus.next()
        };
    }

    pub fn focus_field(&mut self, field: Field) {
        self.focus = Focus::Field(field);
    }

    pub fn focus_button(&mut self, button: Button) {
        self.focus = Focus::Button(button);
    }
}

/// Everything needed to start a client. The password lives here for the
/// lifetime of the spawn call and is never serialized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchRequest {
    pub target: Target,
    pub password: String,
}

impl LaunchRequest {
    /// Start the same binary in direct-connect mode. The manager stays open.
    pub fn spawn(&self) -> std::io::Result<u32> {
        let exe = std::env::current_exe()?;
        let mut child = client_command(&exe, self).spawn()?;
        let pid = child.id();
        // Reap the child from a detached thread: waiting in the UI thread
        // would freeze the manager for the duration of a remote session.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(pid)
    }
}

/// The child command, split out so tests can inspect it without spawning.
pub fn client_command(exe: &Path, request: &LaunchRequest) -> Command {
    let mut command = Command::new(exe);
    command
        .arg("--host")
        .arg(&request.target.host)
        .arg("--port")
        .arg(request.target.port.to_string())
        .arg("--user")
        .arg(&request.target.user);
    // Raw evdev is the only Linux backend that delivers pinch/rotate.
    #[cfg(target_os = "linux")]
    command.arg("--evdev");
    // The password never appears in argv (visible through /proc) or on disk.
    command.env("SS_PASSWORD", &request.password);
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn book(targets: Vec<Target>) -> TargetBook {
        TargetBook {
            targets,
            warnings: Vec::new(),
            path: Some(std::env::temp_dir().join("openssc-manager-unused-targets.conf")),
        }
    }

    fn seed_state() -> ManagerState {
        ManagerState::new(book(vec![Target::seed()]))
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "openssc-manager-state-{name}-{}-{}.conf",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn tab_cycles_list_fields_buttons_and_back() {
        let mut state = seed_state();
        assert_eq!(state.focus, Focus::List);
        let mut seen = vec![state.focus];
        for _ in 0..9 {
            state.tab(false);
            seen.push(state.focus);
        }
        assert_eq!(
            seen,
            vec![
                Focus::List,
                Focus::Field(Field::Name),
                Focus::Field(Field::Host),
                Focus::Field(Field::Port),
                Focus::Field(Field::User),
                Focus::Field(Field::Password),
                Focus::Button(Button::Add),
                Focus::Button(Button::Save),
                Focus::Button(Button::Delete),
                Focus::Button(Button::Connect),
            ]
        );
        // The tenth Tab wraps to the list; Shift+Tab goes back one step.
        state.tab(false);
        assert_eq!(state.focus, Focus::List);
        state.tab(true);
        assert_eq!(state.focus, Focus::Button(Button::Connect));
    }

    #[test]
    fn arrows_move_selection_and_load_the_form_without_wrapping() {
        let mut state = ManagerState::new(book(vec![
            Target::new("one", "one.local", 5900, "tai"),
            Target::new("two", "two.local", 5901, "tai"),
            Target::new("three", "three.local", 5902, "guest"),
        ]));
        state.select_delta(1);
        assert_eq!(state.selected, Some(1));
        assert_eq!(state.form.host, "two.local");
        assert_eq!(state.form.port, "5901");
        state.select_delta(10);
        assert_eq!(state.selected, Some(2));
        state.select_delta(-10);
        assert_eq!(state.selected, Some(0));
        assert_eq!(state.form.host, "one.local");
    }

    #[test]
    fn typing_edits_only_the_focused_field_and_backspace_deletes() {
        let mut state = seed_state();
        state.edit_char('x'); // focus is the list: ignored
        assert_eq!(state.form.name, "mm.local");
        state.focus_field(Field::Name);
        state.edit_char('!');
        state.edit_char(' ');
        assert_eq!(state.form.name, "mm.local! ");
        assert!(state.dirty);
        state.backspace();
        assert_eq!(state.form.name, "mm.local!");
        state.backspace();
        assert_eq!(state.form.name, "mm.local");
        state.focus_field(Field::Port);
        state.edit_char('a');
        assert_eq!(state.form.port, "5900");
        state.clear_field();
        assert_eq!(state.form.port, "");
        state.edit_char('5');
        state.edit_char('9');
        assert_eq!(state.form.port, "59");
    }

    #[test]
    fn add_creates_an_editable_draft_and_selects_it() {
        let mut state = seed_state();
        let before = state.targets.len();
        state.add();
        assert_eq!(state.targets.len(), before + 1);
        assert_eq!(state.selected, Some(before));
        assert_eq!(state.focus, Focus::Field(Field::Name));
        assert!(state.dirty);
        assert_eq!(state.form.name, "New target");
        assert_eq!(state.form.port, "5900");
        assert!(state.form.to_target().is_err()); // no host yet
    }

    #[test]
    fn delete_removes_the_selection_and_clamps() {
        let mut state = ManagerState::new(book(vec![
            Target::new("one", "one.local", 5900, "tai"),
            Target::new("two", "two.local", 5900, "tai"),
        ]));
        state.select(1);
        state.delete_selected();
        assert_eq!(state.targets.len(), 1);
        assert_eq!(state.selected, Some(0));
        assert_eq!(state.form.name, "one");
        state.delete_selected();
        assert!(state.targets.is_empty());
        assert_eq!(state.selected, None);
        assert_eq!(state.focus, Focus::List);
        assert!(state.list_dirty);
        assert!(state.is_dirty());
    }

    #[test]
    fn save_refuses_invalid_form_and_writes_valid_targets() {
        let path = temp_path("save");
        let mut state = ManagerState::new(TargetBook {
            targets: vec![Target::seed()],
            warnings: Vec::new(),
            path: Some(path.clone()),
        });
        state.focus_field(Field::Host);
        state.clear_field();
        assert!(!state.save());
        assert!(state.status.contains("host is required"));
        assert!(!path.exists(), "invalid state must not create the file");

        state.form.host = "mm.local".to_string();
        state.form.name = "Lab Mac".to_string();
        state.dirty = true;
        assert!(state.save());
        assert!(!state.dirty);
        assert!(!state.list_dirty);
        assert!(!state.is_dirty());
        let parsed = targets::parse(&fs::read_to_string(&path).unwrap());
        assert_eq!(parsed.targets.len(), 1);
        assert_eq!(parsed.targets[0].name, "Lab Mac");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn save_reports_a_bad_port_without_touching_the_file() {
        let path = temp_path("port");
        let mut state = ManagerState::new(TargetBook {
            targets: vec![Target::seed()],
            warnings: Vec::new(),
            path: Some(path.clone()),
        });
        state.focus_field(Field::Port);
        state.clear_field();
        assert!(!state.save());
        assert!(state.status.contains("port"));
        assert!(!path.exists());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn connect_request_validates_and_keeps_the_password_out_of_the_target() {
        let mut state = seed_state();
        assert!(state.connect_request().is_ok());
        state.focus_field(Field::Host);
        state.clear_field();
        assert!(state.connect_request().is_err());
        state.form.host = "mm.local".to_string();
        state.form.password = "password".to_string();
        let request = state.connect_request().unwrap();
        assert_eq!(request.target.host, "mm.local");
        assert_eq!(request.password, "password");
        // The serializable side has no password field at all.
        let text = targets::serialize(&[request.target.clone()]);
        assert!(!text.contains("password=password"));
    }

    #[test]
    fn password_field_is_masked_and_never_persisted() {
        let path = temp_path("password");
        let mut state = ManagerState::new(TargetBook {
            targets: vec![Target::seed()],
            warnings: Vec::new(),
            path: Some(path.clone()),
        });
        state.focus_field(Field::Password);
        for c in "s3cret".chars() {
            state.edit_char(c);
        }
        assert_eq!(state.form.password, "s3cret");
        assert_eq!(state.form.masked_password(), "******");
        assert!(state.save());
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("s3cret"));
        // Loading the saved file puts the password back to empty.
        let book = targets::load(&path).unwrap();
        let reloaded = ManagerState::new(book);
        assert_eq!(reloaded.form.password, "");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn client_command_uses_host_port_user_and_env_only() {
        let request = LaunchRequest {
            target: Target::new("Lab", "mm.local", 5900, "tai"),
            password: "password".to_string(),
        };
        let command = client_command(Path::new("/usr/bin/openssc"), &request);
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            &args[..6],
            &["--host", "mm.local", "--port", "5900", "--user", "tai"]
        );
        #[cfg(target_os = "linux")]
        assert!(args.contains(&"--evdev".to_string()));
        assert!(!args.iter().any(|a| a.contains("password")));
        assert!(!args.iter().any(|a| a == "s3cret"));
        let env: Vec<_> = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(env.contains(&("SS_PASSWORD".to_string(), Some("password".to_string()))));
    }
}
