use std::{
    collections::HashMap,
    io::Write,
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
};

use anyhow::{Context, Result, bail};

/// A `[env]` value of this form means the secret lives in the OS keychain under that service.
pub const KEYCHAIN_PREFIX: &str = "keychain:";

/// Provider keys and settings: `<home>/config.toml` `[env]` table, overridden by the process environment.
#[derive(Clone, Default)]
pub struct Env(Arc<HashMap<String, String>>);

impl Env {
    pub fn load(home: &Path) -> Result<Self> {
        let path = home.join("config.toml");
        let mut vars = HashMap::new();
        if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            let config: toml::Table =
                toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
            if let Some(toml::Value::Table(env)) = config.get("env") {
                for (k, v) in env {
                    let v = match v {
                        toml::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    let v = match v.strip_prefix(KEYCHAIN_PREFIX) {
                        Some(service) => match keychain_get(service, k) {
                            Some(secret) => secret,
                            None => {
                                tracing::warn!(
                                    "{k} names keychain service {service} but the value could not be \
                                     read; run `ai-media-router keys set {k}`"
                                );
                                continue;
                            }
                        },
                        None => v,
                    };
                    vars.insert(k.clone(), v);
                }
            }
            if loose_permissions(&path) {
                tracing::warn!(
                    "{} is readable by other users; run `chmod 600 {}`",
                    path.display(),
                    path.display()
                );
            }
        }
        vars.extend(std::env::vars());
        Ok(Self(Arc::new(vars)))
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str).filter(|v| !v.is_empty())
    }

    pub fn from_map(vars: impl IntoIterator<Item = (String, String)>) -> Self {
        Self(Arc::new(vars.into_iter().collect()))
    }

    #[cfg(test)]
    pub fn from_pairs<const N: usize>(pairs: [(&str, &str); N]) -> Self {
        Self::from_map(pairs.map(|(k, v)| (k.to_owned(), v.to_owned())))
    }
}

/// The raw `[env]` table of `<home>/config.toml`, `keychain:` markers unresolved; empty when absent.
pub fn raw_env(home: &Path) -> HashMap<String, String> {
    let text = std::fs::read_to_string(home.join("config.toml")).unwrap_or_default();
    let config: toml::Table = toml::from_str(&text).unwrap_or_default();
    let Some(toml::Value::Table(env)) = config.get("env") else {
        return HashMap::new();
    };
    env.iter()
        .map(|(k, v)| {
            let v = match v {
                toml::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            (k.clone(), v)
        })
        .collect()
}

/// True when `path` is group- or world-accessible; always false off unix.
pub fn loose_permissions(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o077 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// Where a key is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Store {
    Auto,
    Keychain,
    File,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Stored {
    pub name: String,
    /// `"keychain"` or `"file"`.
    pub store: &'static str,
    pub backend: String,
    pub masked: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Removed {
    pub name: String,
    /// `"keychain"` or `"file"` — where the key was.
    pub store: &'static str,
    pub backend: String,
}

/// The service for a home: a stable, path-derived name that never contains a secret.
fn keychain_service(home: &Path) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(home.to_string_lossy().as_bytes());
    format!("ai-media-router/{}", &hex::encode(digest)[..12])
}

/// The OS keychain when one is available, else `None`.
pub fn keychain_name() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some("macOS keychain")
    } else if cfg!(target_os = "linux") && on_path("secret-tool") {
        Some("Secret Service (secret-tool)")
    } else {
        None
    }
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| dir.join(program).is_file())
    })
}

/// Reads one secret from the keychain; stdout/stderr are never logged.
fn keychain_get(service: &str, account: &str) -> Option<String> {
    keychain_name()?;
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("/usr/bin/security", vec!["find-generic-password", "-s", service, "-a", account, "-w"])
    } else {
        ("secret-tool", vec!["lookup", "service", service, "account", account])
    };
    let out = Command::new(program).args(&args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let mut text = String::from_utf8(out.stdout).ok()?;
    while text.ends_with('\n') || text.ends_with('\r') {
        text.pop();
    }
    Some(text)
}

/// The value travels over stdin (or as hex), never in argv.
fn keychain_store(service: &str, account: &str, value: &str) -> Result<()> {
    let name = keychain_name().ok_or_else(|| {
        anyhow::anyhow!(
            "no OS keychain on this system; run again with --store file to keep it in config.toml (mode 0600)"
        )
    })?;
    let (program, args, stdin_text): (&str, Vec<String>, String) = if cfg!(target_os = "macos") {
        let line = format!("add-generic-password -U -s {service} -a {account} -X {}\n", hex::encode(value.as_bytes()));
        ("/usr/bin/security", vec!["-i".to_owned()], line)
    } else {
        (
            "secret-tool",
            vec!["store".to_owned(), "--label".to_owned(), format!("ai-media-router {account}"), "service".to_owned(), service.to_owned(), "account".to_owned(), account.to_owned()],
            value.to_owned(),
        )
    };
    let mut child = Command::new(program)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot start {program}"))?;
    child.stdin.as_mut().unwrap().write_all(stdin_text.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        bail!("{name} refused to store {account}: {err}; run again with --store file to keep it in config.toml (mode 0600)");
    }
    Ok(())
}

/// An already-missing item is not an error; anything else names the backend and the fix.
fn keychain_delete(service: &str, account: &str) -> Result<()> {
    let Some(name) = keychain_name() else {
        return Ok(());
    };
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("/usr/bin/security", vec!["delete-generic-password", "-s", service, "-a", account])
    } else {
        ("secret-tool", vec!["clear", "service", service, "account", account])
    };
    let out = Command::new(program).args(&args).output()?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    if err.to_lowercase().contains("could not be found")
        || err.to_lowercase().contains("not found")
        || err.to_lowercase().contains("no such")
    {
        return Ok(());
    }
    bail!("{name} could not delete {account}: {}; remove the item `{service}` / `{account}` with the keychain app", err.trim());
}

/// `[A-Z][A-Z0-9_]*`.
pub fn validate_name(name: &str) -> Result<()> {
    let ok = name.chars().enumerate().all(|(i, c)| {
        if i == 0 { c.is_ascii_uppercase() } else { c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' }
    });
    if name.is_empty() || !ok {
        bail!("`{name}` is not a valid key name; use [A-Z][A-Z0-9_]* (e.g. OPENAI_API_KEY)");
    }
    Ok(())
}

pub fn validate_value(value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("no key was entered; run the command again and paste the key at the prompt");
    }
    if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("a key is one line without spaces; check what you pasted");
    }
    Ok(())
}

/// `…` plus the last 4 characters when long enough, else just `…`.
pub fn mask(value: &str) -> String {
    if value.chars().count() >= 12 {
        let tail: String = value.chars().rev().take(4).collect();
        let tail: String = tail.chars().rev().collect();
        format!("…{tail}")
    } else {
        "…".to_owned()
    }
}

/// Stores `name` in the keychain (a `keychain:<service>` marker) or in `[env]` (the value).
pub fn set_key(home: &Path, name: &str, value: &str, store: Store) -> Result<Stored> {
    validate_name(name)?;
    validate_value(value)?;

    let path = home.join("config.toml");
    let service = keychain_service(home);
    let old_marker = env_value(home, name).and_then(|v| v.strip_prefix(KEYCHAIN_PREFIX).map(str::to_owned));

    let use_keychain = match store {
        Store::Auto => keychain_name().is_some(),
        Store::Keychain => true,
        Store::File => false,
    };

    let (raw, store_name, backend): (String, &'static str, String) = if use_keychain {
        let backend = keychain_name().ok_or_else(|| {
            anyhow::anyhow!(
                "no OS keychain on this system; run again with --store file to keep it in config.toml (mode 0600)"
            )
        })?;
        keychain_store(&service, name, value)?;
        (format!("{KEYCHAIN_PREFIX}{service}"), "keychain", backend.to_owned())
    } else {
        (value.to_owned(), "file", path.display().to_string())
    };

    let line = format!("{name} = {}", toml::Value::String(raw.clone()));
    let text = read_existing(&path);
    let new_text = edit_env_line(&text, name, Some(&line))?;
    verify_env(&path, &new_text, name, Some(&raw))?;
    write_atomic(&path, &new_text)?;

    // A previous keychain marker is replaced by a file value: drop the item once the file is written.
    if !use_keychain
        && let Some(old) = old_marker
    {
        keychain_delete(&old, name)?;
    }

    Ok(Stored { name: name.to_owned(), store: store_name, backend, masked: mask(value) })
}

/// Drops the `name` line from `[env]` and, when it held a marker, the keychain item.
pub fn remove_key(home: &Path, name: &str) -> Result<Removed> {
    validate_name(name)?;
    let path = home.join("config.toml");
    let current = env_value(home, name);
    let old_marker = current.as_ref().and_then(|v| v.strip_prefix(KEYCHAIN_PREFIX).map(str::to_owned));
    if current.is_none() {
        if std::env::var(name).is_ok() {
            bail!("{name} is not in {} but is set in the process environment; unset it in your shell profile", path.display());
        }
        bail!("{name} is not in {}; nothing to remove", path.display());
    }

    let text = read_existing(&path);
    let new_text = edit_env_line(&text, name, None)?;
    verify_env(&path, &new_text, name, None)?;
    write_atomic(&path, &new_text)?;

    let (store, backend) = match &old_marker {
        Some(marker) => {
            keychain_delete(marker, name)?;
            ("keychain", keychain_name().unwrap_or("keychain").to_owned())
        }
        None => ("file", path.display().to_string()),
    };
    Ok(Removed { name: name.to_owned(), store, backend })
}

/// The current `[env]` value for `name`, if any.
fn env_value(home: &Path, name: &str) -> Option<String> {
    raw_env(home).remove(name)
}

fn read_existing(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// The key of an `[env]` line, or `None` for comments, blanks and other content.
fn env_line_key(line: &str) -> Option<&str> {
    let line = line.trim();
    if line.starts_with('#') {
        return None;
    }
    let (key, _) = line.split_once('=')?;
    let key = key.trim();
    let key = key.strip_prefix('"').and_then(|k| k.strip_suffix('"')).unwrap_or(key);
    (!key.is_empty()).then_some(key)
}

/// Replaces or removes the `name` line inside `[env]`; comments and other tables survive.
fn edit_env_line(text: &str, name: &str, replacement: Option<&str>) -> Result<String> {
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    match lines.iter().position(|l| l.trim() == "[env]") {
        Some(header) => {
            let end = lines[header + 1..]
                .iter()
                .position(|l| l.trim_start().starts_with('['))
                .map_or(lines.len(), |i| header + 1 + i);
            let slot = (header + 1..end).find(|&i| env_line_key(&lines[i]) == Some(name));
            match (slot, replacement) {
                (Some(i), Some(line)) => lines[i] = line.to_owned(),
                (Some(i), None) => {
                    lines.remove(i);
                }
                (None, Some(line)) => lines.insert(header + 1, line.to_owned()),
                (None, None) => {}
            }
        }
        None => {
            if let Some(line) = replacement {
                lines.push("[env]".to_owned());
                lines.push(line.to_owned());
            }
        }
    }
    let mut out = lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// The edited text must parse and carry exactly the expected `[env]` value before anything is written.
fn verify_env(path: &Path, text: &str, name: &str, expected: Option<&str>) -> Result<()> {
    let fail = || anyhow::anyhow!("could not edit {} safely: edit its [env] table by hand", path.display());
    let config: toml::Table = toml::from_str(text).map_err(|_| fail())?;
    let actual = config.get("env").and_then(|e| e.as_table()).and_then(|t| t.get(name)).and_then(|v| v.as_str());
    let ok = match expected {
        Some(exp) => actual == Some(exp),
        None => actual.is_none(),
    };
    if !ok {
        bail!(fail());
    }
    Ok(())
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let tmp = path.with_file_name(format!("config.toml.tmp-{}", uuid::Uuid::new_v4().simple()));
    let mut file = open_new(&tmp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn open_new(path: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    Ok(std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?)
}

#[cfg(not(unix))]
fn open_new(path: &Path) -> Result<std::fs::File> {
    Ok(std::fs::File::create(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home() -> std::path::PathBuf {
        let home = std::env::temp_dir().join(format!("ai-media-router-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    #[test]
    fn set_preserves_comments_and_other_tables() {
        let home = temp_home();
        std::fs::write(
            home.join("config.toml"),
            "# top comment\n[env]\n# keep me\nEXISTING = \"old\"\n\n[other]\nx = 1\n",
        )
        .unwrap();
        let stored = set_key(&home, "NEW_KEY", "secret-value", Store::File).unwrap();
        assert_eq!(stored.store, "file");
        assert_eq!(stored.masked, "…alue");
        let text = std::fs::read_to_string(home.join("config.toml")).unwrap();
        for kept in ["# top comment", "# keep me", "[other]", "x = 1", "EXISTING = \"old\"", "NEW_KEY = \"secret-value\""] {
            assert!(text.contains(kept), "{kept} lost:\n{text}");
        }
        let env = Env::load(&home).unwrap();
        assert_eq!(env.get("NEW_KEY"), Some("secret-value"));
        assert_eq!(env.get("EXISTING"), Some("old"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn set_replaces_and_remove_drops() {
        let home = temp_home();
        set_key(&home, "KEY", "one", Store::File).unwrap();
        set_key(&home, "KEY", "two", Store::File).unwrap();
        assert_eq!(std::fs::read_to_string(home.join("config.toml")).unwrap().matches("KEY =").count(), 1);
        assert_eq!(Env::load(&home).unwrap().get("KEY"), Some("two"));
        let removed = remove_key(&home, "KEY").unwrap();
        assert_eq!((removed.name.as_str(), removed.store), ("KEY", "file"));
        assert_eq!(Env::load(&home).unwrap().get("KEY"), None);
        let err = remove_key(&home, "KEY").unwrap_err().to_string();
        assert!(err.contains("nothing to remove"), "{err}");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn no_env_table_appends() {
        let home = temp_home();
        std::fs::write(home.join("config.toml"), "[other]\nx = 1\n").unwrap();
        set_key(&home, "KEY", "v", Store::File).unwrap();
        let text = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(text.contains("[env]\nKEY = \"v\"\n"), "{text}");
        assert_eq!(Env::load(&home).unwrap().get("KEY"), Some("v"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[cfg(unix)]
    #[test]
    fn written_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let home = temp_home();
        set_key(&home, "KEY", "v", Store::File).unwrap();
        let mode = std::fs::metadata(home.join("config.toml")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn unresolvable_keychain_marker_is_unset() {
        let home = temp_home();
        std::fs::write(home.join("config.toml"), "[env]\nKEY = \"keychain:does-not-exist-xyz\"\n").unwrap();
        let env = Env::load(&home).unwrap();
        assert_eq!(env.get("KEY"), None);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn mask_hides_long_and_short() {
        assert_eq!(mask("sk-1234567890abcd"), "…abcd");
        assert_eq!(mask("short"), "…");
    }
}
