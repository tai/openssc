//! Saved connection targets for the manager window.
//!
//! The on-disk format is deliberately boring: an INI-like list of blocks,
//! written by this module and nothing else. Passwords are *not* part of this
//! model and never reach the file; the manager keeps the current password in
//! the session's form and hands it to the child through `SS_PASSWORD`.
//!
//! ```text
//! # OpenSSC connection targets (v1)
//! [target]
//! name=mm.local
//! host=mm.local
//! port=5900
//! user=tai
//! ```
//!
//! The file is created `0600` in `$XDG_CONFIG_HOME/openssc/targets.conf`
//! (normally `~/.config/openssc/targets.conf`). `SS_TARGETS_FILE` overrides the
//! path for tests and one-off runs.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The default port for Apple Screen Sharing.
pub const DEFAULT_PORT: u16 = 5900;
const MAX_NAME_LEN: usize = 64;
const MAX_HOST_LEN: usize = 253;
const MAX_USER_LEN: usize = 64;

/// One saved connection. No password field exists here on purpose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
}

impl Target {
    pub fn new(
        name: impl Into<String>,
        host: impl Into<String>,
        port: u16,
        user: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            host: host.into(),
            port,
            user: user.into(),
        }
    }

    /// The single target written when the config file does not exist yet, so
    /// the lab can be tried from the manager without any setup.
    pub fn seed() -> Self {
        Self::new("mm.local", "mm.local", DEFAULT_PORT, "tai")
    }

    /// Empty draft for the Add button.
    pub fn draft() -> Self {
        Self::new("New target", "", DEFAULT_PORT, "tai")
    }

    /// Human-readable validation errors. An empty vector means connect/save is
    /// allowed. The manager shows the first error in its status line.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let name = self.name.trim();
        if name.is_empty() {
            errors.push("name is required".to_string());
        } else if name.chars().count() > MAX_NAME_LEN {
            errors.push(format!("name is longer than {MAX_NAME_LEN} characters"));
        } else if has_bad_text(name) {
            errors.push("name contains a control character".to_string());
        }

        let host = self.host.trim();
        if host.is_empty() {
            errors.push("host is required".to_string());
        } else if host.chars().count() > MAX_HOST_LEN {
            errors.push(format!("host is longer than {MAX_HOST_LEN} characters"));
        } else if host.chars().any(|c| c.is_whitespace()) || has_bad_text(host) {
            errors.push("host must not contain spaces".to_string());
        }

        if self.port == 0 {
            errors.push("port must be between 1 and 65535".to_string());
        }

        let user = self.user.trim();
        if user.is_empty() {
            errors.push("user is required".to_string());
        } else if user.chars().count() > MAX_USER_LEN {
            errors.push(format!("user is longer than {MAX_USER_LEN} characters"));
        } else if user.chars().any(|c| c.is_whitespace()) || has_bad_text(user) {
            errors.push("user must not contain spaces".to_string());
        }
        errors
    }
}

fn has_bad_text(s: &str) -> bool {
    s.chars().any(|c| c.is_control())
}

/// A loaded target list plus any non-fatal parse complaints.
#[derive(Clone, Debug, Default)]
pub struct TargetBook {
    pub targets: Vec<Target>,
    pub warnings: Vec<String>,
    pub path: Option<PathBuf>,
}

/// Result of parsing the line format.
#[derive(Clone, Debug, Default)]
pub struct Parsed {
    pub targets: Vec<Target>,
    pub warnings: Vec<String>,
}

/// Parse the small block format. The parser is intentionally forgiving: a bad
/// block is skipped with a warning instead of taking the whole file down.
pub fn parse(text: &str) -> Parsed {
    let mut out = Parsed::default();
    let mut pending = false;
    let mut name: Option<String> = None;
    let mut host: Option<String> = None;
    let mut port: Option<u16> = None;
    let mut user: Option<String> = None;

    let flush = |name: &mut Option<String>,
                 host: &mut Option<String>,
                 port: &mut Option<u16>,
                 user: &mut Option<String>,
                 warnings: &mut Vec<String>,
                 targets: &mut Vec<Target>| {
        if !(name.is_some() || host.is_some() || port.is_some() || user.is_some()) {
            return;
        }
        let label = name.clone().unwrap_or_else(|| "<unnamed>".to_string());
        let missing = [
            name.is_none().then_some("name"),
            host.is_none().then_some("host"),
            port.is_none().then_some("port"),
            user.is_none().then_some("user"),
        ];
        let fields = (name.take(), host.take(), port.take(), user.take());
        match fields {
            (Some(name), Some(host), Some(port), Some(user)) => {
                let target = Target {
                    name,
                    host,
                    port,
                    user,
                };
                let errors = target.validate();
                if errors.is_empty() {
                    targets.push(target);
                } else {
                    warnings.push(format!("target {label:?}: {}", errors.join("; ")));
                }
            }
            _ => warnings.push(format!(
                "target {label:?}: missing {}",
                missing.into_iter().flatten().collect::<Vec<_>>().join(", ")
            )),
        }
    };

    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            if line.is_empty() {
                flush(
                    &mut name,
                    &mut host,
                    &mut port,
                    &mut user,
                    &mut out.warnings,
                    &mut out.targets,
                );
                pending = false;
            }
            continue;
        }
        if line.eq_ignore_ascii_case("[target]") {
            flush(
                &mut name,
                &mut host,
                &mut port,
                &mut user,
                &mut out.warnings,
                &mut out.targets,
            );
            pending = true;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            out.warnings
                .push(format!("line {}: expected key=value", index + 1));
            continue;
        };
        if !pending {
            pending = true;
        }
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        match key.as_str() {
            "name" => name = Some(value),
            "host" => host = Some(value),
            "port" => match value.parse::<u16>() {
                Ok(p) if p > 0 => port = Some(p),
                _ => out.warnings.push(format!(
                    "line {}: port {value:?} is not between 1 and 65535",
                    index + 1
                )),
            },
            "user" => user = Some(value),
            other => out
                .warnings
                .push(format!("line {}: unknown key {other:?}", index + 1)),
        }
    }
    flush(
        &mut name,
        &mut host,
        &mut port,
        &mut user,
        &mut out.warnings,
        &mut out.targets,
    );
    out
}

/// Serialise targets deterministically. Only name/host/port/user are written.
pub fn serialize(targets: &[Target]) -> String {
    let mut out = String::from(
        "# OpenSSC connection targets (v1)\n\
         # Fields: name, host, port, user. Passwords are never stored.\n",
    );
    for target in targets {
        out.push_str("\n[target]\n");
        out.push_str(&format!("name={}\n", target.name.trim()));
        out.push_str(&format!("host={}\n", target.host.trim()));
        out.push_str(&format!("port={}\n", target.port));
        out.push_str(&format!("user={}\n", target.user.trim()));
    }
    out
}

/// Where the target list lives unless `SS_TARGETS_FILE` says otherwise.
pub fn default_config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("SS_TARGETS_FILE") {
        return PathBuf::from(path);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("openssc").join("targets.conf")
}

/// Load a target file. A missing file is not an error: it is created with the
/// single seed target so a new install has something to connect to.
pub fn load(path: &Path) -> io::Result<TargetBook> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let parsed = parse(&text);
            Ok(TargetBook {
                targets: parsed.targets,
                warnings: parsed.warnings,
                path: Some(path.to_path_buf()),
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let targets = vec![Target::seed()];
            save(path, &targets)?;
            Ok(TargetBook {
                targets,
                warnings: Vec::new(),
                path: Some(path.to_path_buf()),
            })
        }
        Err(error) => Err(error),
    }
}

/// Write the target list atomically: temp file next to the destination, mode
/// 0600, then rename. The parent directory is created 0700 when needed.
pub fn save(path: &Path, targets: &[Target]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            create_private_dir(parent)?;
        }
    }
    let text = serialize(targets);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let tmp = parent.join(format!(
        ".targets.conf.tmp.{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));

    let result = (|| -> io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        drop(file);
        fs::rename(&tmp, path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_temp_dir<T>(name: &str, f: impl FnOnce(&Path) -> T) -> T {
        let dir = std::env::temp_dir().join(format!(
            "openssc-manager-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let result = f(&dir);
        let _ = fs::remove_dir_all(&dir);
        result
    }

    #[test]
    fn round_trip_keeps_name_host_port_user() {
        let targets = vec![
            Target::new("Lab Mac", "mm.local", 5900, "tai"),
            Target::new("workstation", "192.168.3.20", 5901, "tai"),
        ];
        let text = serialize(&targets);
        let parsed = parse(&text);
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
        assert_eq!(parsed.targets, targets);
    }

    #[test]
    fn parser_accepts_headerless_blocks_and_comments() {
        let text = "# a comment\nname=one\nhost=one.local\nport=5900\nuser=tai\n\n\
                    ; second\ntarget=ignored\nname=two\nhost=two.local\nport=5901\nuser=guest\n";
        // `target=ignored` is an unknown key, not a block marker; it must not
        // lose the following fields.
        let parsed = parse(text);
        assert_eq!(parsed.targets.len(), 2);
        assert_eq!(parsed.targets[0].host, "one.local");
        assert_eq!(parsed.targets[1].host, "two.local");
        assert!(parsed.warnings.iter().any(|w| w.contains("unknown key")));
    }

    #[test]
    fn parser_skips_invalid_blocks_with_a_warning() {
        let text = "[target]\nname=bad\nhost=bad.local\nport=0\nuser=tai\n\n\
                    [target]\nname=ok\nhost=ok.local\nport=5900\nuser=tai\n";
        let parsed = parse(text);
        assert_eq!(parsed.targets.len(), 1);
        assert_eq!(parsed.targets[0].name, "ok");
        assert!(parsed.warnings.iter().any(|w| w.contains("port")));
    }

    #[test]
    fn parser_reports_missing_fields() {
        let parsed = parse("[target]\nname=half\nhost=half.local\n");
        assert!(parsed.targets.is_empty());
        assert!(parsed.warnings.iter().any(|w| w.contains("missing")));
    }

    #[test]
    fn validation_checks_required_fields_and_port() {
        assert!(Target::new("", "mm.local", 5900, "tai").validate()[0].contains("name"));
        assert!(Target::new("x", "", 5900, "tai").validate()[0].contains("host"));
        assert!(Target::new("x", "bad host", 5900, "tai")
            .validate()
            .iter()
            .any(|e| e.contains("spaces")));
        assert!(Target::new("x", "mm.local", 0, "tai")
            .validate()
            .iter()
            .any(|e| e.contains("port")));
        assert!(Target::new("x", "mm.local", 5900, "").validate()[0].contains("user"));
        assert!(Target::seed().validate().is_empty());
    }

    #[test]
    fn missing_file_is_seeded_and_written_0600() {
        with_temp_dir("seed", |dir| {
            let path = dir.join("openssc").join("targets.conf");
            let book = load(&path).unwrap();
            assert_eq!(book.targets, vec![Target::seed()]);
            assert!(path.exists());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600);
                let dir_mode = fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(dir_mode, 0o700);
            }
            let text = fs::read_to_string(&path).unwrap();
            assert!(!text.lines().any(|line| line
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("password")));
        });
    }

    #[test]
    fn save_replaces_the_file_and_never_writes_a_password_field() {
        with_temp_dir("save", |dir| {
            let path = dir.join("targets.conf");
            let targets = vec![Target::new("one", "one.local", 5900, "tai")];
            save(&path, &targets).unwrap();
            save(&path, &[Target::new("two", "two.local", 5901, "tai")]).unwrap();
            let parsed = parse(&fs::read_to_string(&path).unwrap());
            assert_eq!(parsed.targets.len(), 1);
            assert_eq!(parsed.targets[0].name, "two");
            assert!(!fs::read_to_string(&path).unwrap().contains("password"));
        });
    }

    #[test]
    fn config_path_env_override_is_honoured() {
        // This test touches the process environment, so keep it self-contained
        // and restore the previous value.
        let old = std::env::var_os("SS_TARGETS_FILE");
        std::env::set_var("SS_TARGETS_FILE", "/tmp/openssc-targets-test");
        assert_eq!(
            default_config_path(),
            PathBuf::from("/tmp/openssc-targets-test")
        );
        match old {
            Some(value) => std::env::set_var("SS_TARGETS_FILE", value),
            None => std::env::remove_var("SS_TARGETS_FILE"),
        }
    }
}
