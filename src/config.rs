#![allow(dead_code)] // API surface for 010 auto-approval; not yet wired into a command

// Config file management for the Claude Code CLI's ~/.claude.json. This module
// implements the same locked read-modify-write protocol the CLI uses internally
// (wDq() at cli.js:527184-527241) so Puzzle can safely mutate the config without
// stomping concurrent Claude instances.
//
// The locking mechanism is proper-lockfile, which uses mkdir as an atomic lock
// primitive. The lockfile is a directory, not a file — ~/.claude.json.lock/ is
// created to acquire and removed to release. This matches the CLI's YDq.lockSync
// call at cli.js:527192.
//
// The config file is sparse — only keys that differ from the CLI's built-in
// defaults are written to disk. When we read, we get a partial object. When we
// write, we preserve that sparseness. We never expand defaults into the file.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Errors from config operations.
#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Json(serde_json::Error),
    LockContention,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "io error: {}", e),
            ConfigError::Json(e) => write!(f, "json error: {}", e),
            ConfigError::LockContention => write!(f, "lock contention: lock directory already exists"),
        }
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

impl From<serde_json::Error> for ConfigError {
    fn from(e: serde_json::Error) -> Self {
        ConfigError::Json(e)
    }
}

/// Acquire a mkdir-based lock, matching proper-lockfile's lockSync. Returns
/// the lock directory path on success. The caller must remove this directory
/// to release the lock.
fn acquire_lock(config_path: &Path) -> Result<PathBuf, ConfigError> {
    let lock_path = config_path.with_extension("json.lock");
    match fs::create_dir(&lock_path) {
        Ok(()) => Ok(lock_path),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(ConfigError::LockContention)
        }
        Err(e) => Err(ConfigError::Io(e)),
    }
}

/// Release the lock by removing the lock directory.
fn release_lock(lock_path: &Path) {
    let _ = fs::remove_dir(lock_path);
}

/// Read the config file, returning the parsed JSON object. If the file does
/// not exist, returns an empty object. This matches R11() behavior at
/// cli.js:527274 — missing file returns defaults.
fn read_config(config_path: &Path) -> Result<Value, ConfigError> {
    match fs::read_to_string(config_path) {
        Ok(content) => Ok(serde_json::from_str(&content)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(Value::Object(serde_json::Map::new()))
        }
        Err(e) => Err(ConfigError::Io(e)),
    }
}

/// Write the config file at mode 0600 (decimal 384), matching cli.js:527237.
fn write_config(config_path: &Path, value: &Value) -> Result<(), ConfigError> {
    let content = serde_json::to_string_pretty(value)?;
    fs::write(config_path, &content)?;
    fs::set_permissions(config_path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Create a timestamped backup of the config file. Keep the 5 most recent
/// backups, delete older ones. Matches cli.js:527218-527232.
fn backup_config(config_path: &Path) -> Result<(), ConfigError> {
    if !config_path.exists() {
        return Ok(());
    }

    let dir = config_path.parent().unwrap();
    let basename = config_path.file_name().unwrap().to_str().unwrap();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let backup_name = format!("{}.backup.{}", basename, timestamp);
    let backup_path = dir.join(&backup_name);

    fs::copy(config_path, &backup_path)?;

    // prune old backups, keep 5 most recent
    let prefix = format!("{}.backup.", basename);
    let mut backups: Vec<String> = fs::read_dir(dir)?
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            if name.starts_with(&prefix) {
                Some(name)
            } else {
                None
            }
        })
        .collect();

    backups.sort();
    backups.reverse();

    for old in backups.iter().skip(5) {
        let _ = fs::remove_file(dir.join(old));
    }

    Ok(())
}

/// The full locked read-modify-write cycle matching wDq() at cli.js:527184.
/// Acquires the mkdir-based lock, re-reads under lock, applies the mutation,
/// backs up, writes at 0600, and releases the lock in all paths. The mutation
/// closure returns true if a write is needed, false to skip. This matches the
/// CLI's identity check at cli.js:527214 — if nothing changed, no write.
pub fn modify_config<F>(config_path: &Path, mutate: F) -> Result<(), ConfigError>
where
    F: FnOnce(&mut Value) -> bool,
{
    let lock_path = acquire_lock(config_path)?;

    let result = (|| {
        let mut config = read_config(config_path)?;
        if !mutate(&mut config) {
            return Ok(());
        }
        backup_config(config_path)?;
        write_config(config_path, &config)?;
        Ok(())
    })();

    release_lock(&lock_path);
    result
}

/// Check whether a directory already has hasTrustDialogAccepted set to true.
pub fn has_trust(config: &Value, directory: &str) -> bool {
    config
        .get("projects")
        .and_then(|p| p.get(directory))
        .and_then(|e| e.get("hasTrustDialogAccepted"))
        .and_then(|v| v.as_bool())
        == Some(true)
}

/// Ensure hasTrustDialogAccepted is set for a directory path. Returns true
/// if the config was modified, false if trust was already set. This is the
/// mutation that 010 needs — write a project entry so the CLI's exact-match
/// trust check (hw() at cli.js:527065) finds it and skips the dialog.
pub fn ensure_trust(config: &mut Value, directory: &str) -> bool {
    if has_trust(config, directory) {
        return false;
    }

    let obj = config.as_object_mut().expect("config must be an object");

    let projects = obj
        .entry("projects")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));

    let project = projects
        .as_object_mut()
        .expect("projects must be an object")
        .entry(directory)
        .or_insert_with(|| Value::Object(serde_json::Map::new()));

    project
        .as_object_mut()
        .expect("project entry must be an object")
        .insert(
            "hasTrustDialogAccepted".to_string(),
            Value::Bool(true),
        );

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Build a config path inside a temp directory.
    fn config_in(dir: &Path) -> PathBuf {
        dir.join(".claude.json")
    }

    #[test]
    fn ensure_trust_on_empty_config() {
        let mut config = Value::Object(serde_json::Map::new());
        let modified = ensure_trust(&mut config, "/Users/alan/pane");

        assert!(modified);
        let accepted = config["projects"]["/Users/alan/pane"]["hasTrustDialogAccepted"]
            .as_bool()
            .unwrap();
        assert!(accepted);
    }

    #[test]
    fn ensure_trust_returns_false_when_already_set() {
        let mut config: Value = serde_json::from_str(
            r#"{
                "projects": {
                    "/Users/alan/pane": {
                        "hasTrustDialogAccepted": true
                    }
                }
            }"#,
        )
        .unwrap();

        let modified = ensure_trust(&mut config, "/Users/alan/pane");
        assert!(!modified);
    }

    #[test]
    fn has_trust_checks_correctly() {
        let config: Value = serde_json::from_str(
            r#"{
                "projects": {
                    "/Users/alan/pane": {
                        "hasTrustDialogAccepted": true
                    },
                    "/Users/alan/code": {
                        "hasTrustDialogAccepted": false
                    }
                }
            }"#,
        )
        .unwrap();

        assert!(has_trust(&config, "/Users/alan/pane"));
        assert!(!has_trust(&config, "/Users/alan/code"));
        assert!(!has_trust(&config, "/Users/alan/other"));
    }

    #[test]
    fn ensure_trust_preserves_existing_projects() {
        let mut config: Value = serde_json::from_str(
            r#"{
                "numStartups": 42,
                "projects": {
                    "/Users/alan/code": {
                        "hasTrustDialogAccepted": true,
                        "allowedTools": ["Bash"]
                    }
                }
            }"#,
        )
        .unwrap();

        ensure_trust(&mut config, "/Users/alan/pane");

        // original project untouched
        assert_eq!(
            config["projects"]["/Users/alan/code"]["hasTrustDialogAccepted"],
            true
        );
        assert!(config["projects"]["/Users/alan/code"]["allowedTools"]
            .as_array()
            .unwrap()
            .len()
            == 1);

        // new project added
        assert_eq!(
            config["projects"]["/Users/alan/pane"]["hasTrustDialogAccepted"],
            true
        );

        // top-level preserved
        assert_eq!(config["numStartups"], 42);
    }

    #[test]
    fn ensure_trust_preserves_existing_project_fields() {
        let mut config: Value = serde_json::from_str(
            r#"{
                "projects": {
                    "/Users/alan/pane": {
                        "allowedTools": ["Bash", "Read"],
                        "mcpServers": {},
                        "hasTrustDialogAccepted": false
                    }
                }
            }"#,
        )
        .unwrap();

        ensure_trust(&mut config, "/Users/alan/pane");

        // trust flipped
        assert_eq!(
            config["projects"]["/Users/alan/pane"]["hasTrustDialogAccepted"],
            true
        );

        // other fields preserved
        assert_eq!(
            config["projects"]["/Users/alan/pane"]["allowedTools"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn modify_config_creates_file_from_nothing() {
        let dir = TempDir::new().unwrap();
        let path = config_in(dir.path());

        modify_config(&path, |config| {
            ensure_trust(config, "/Users/alan/pane")
        })
        .unwrap();

        let content = fs::read_to_string(&path).unwrap();
        let parsed: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(
            parsed["projects"]["/Users/alan/pane"]["hasTrustDialogAccepted"],
            true
        );

        // check mode 0600
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn modify_config_preserves_existing_content() {
        let dir = TempDir::new().unwrap();
        let path = config_in(dir.path());

        // write an existing config
        fs::write(
            &path,
            r#"{"numStartups": 812, "verbose": true, "projects": {}}"#,
        )
        .unwrap();

        modify_config(&path, |config| {
            ensure_trust(config, "/Users/alan/pane")
        })
        .unwrap();

        let content = fs::read_to_string(&path).unwrap();
        let parsed: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["numStartups"], 812);
        assert_eq!(parsed["verbose"], true);
        assert_eq!(
            parsed["projects"]["/Users/alan/pane"]["hasTrustDialogAccepted"],
            true
        );
    }

    #[test]
    fn modify_config_creates_backup() {
        let dir = TempDir::new().unwrap();
        let path = config_in(dir.path());

        // write initial content
        fs::write(&path, r#"{"original": true}"#).unwrap();

        modify_config(&path, |config| {
            ensure_trust(config, "/Users/alan/pane")
        })
        .unwrap();

        // should have a backup file
        let backups: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| {
                let name = e.ok()?.file_name().into_string().ok()?;
                if name.starts_with(".claude.json.backup.") {
                    Some(name)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(backups.len(), 1);

        // backup contains original content
        let backup_content =
            fs::read_to_string(dir.path().join(&backups[0])).unwrap();
        let backup: Value = serde_json::from_str(&backup_content).unwrap();
        assert_eq!(backup["original"], true);
    }

    #[test]
    fn modify_config_prunes_old_backups() {
        let dir = TempDir::new().unwrap();
        let path = config_in(dir.path());

        // create 6 pre-existing backups
        for i in 0..6 {
            let backup = dir
                .path()
                .join(format!(".claude.json.backup.{}", 1000 + i));
            fs::write(&backup, "{}").unwrap();
        }

        fs::write(&path, r#"{"round": 7}"#).unwrap();

        modify_config(&path, |config| {
            ensure_trust(config, "/Users/alan/pane")
        })
        .unwrap();

        // should have 5 backups (pruned oldest, kept 5 most recent including new)
        let backups: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| {
                let name = e.ok()?.file_name().into_string().ok()?;
                if name.starts_with(".claude.json.backup.") {
                    Some(name)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(backups.len(), 5);
    }

    #[test]
    fn lock_contention_returns_error() {
        let dir = TempDir::new().unwrap();
        let path = config_in(dir.path());
        fs::write(&path, "{}").unwrap();

        // simulate a held lock
        let lock_dir = dir.path().join(".claude.json.lock");
        fs::create_dir(&lock_dir).unwrap();

        let result = modify_config(&path, |_| false);
        assert!(matches!(result, Err(ConfigError::LockContention)));

        // lock dir still exists (we didn't steal it)
        assert!(lock_dir.exists());
    }

    #[test]
    fn lock_released_after_success() {
        let dir = TempDir::new().unwrap();
        let path = config_in(dir.path());

        modify_config(&path, |config| {
            ensure_trust(config, "/Users/alan/pane")
        })
        .unwrap();

        let lock_dir = dir.path().join(".claude.json.lock");
        assert!(!lock_dir.exists());
    }

    #[test]
    fn lock_released_after_write_error() {
        let dir = TempDir::new().unwrap();
        let path = config_in(dir.path());

        // create the config path as a directory so writing fails
        fs::create_dir_all(&path).unwrap();

        let result = modify_config(&path, |config| {
            ensure_trust(config, "/Users/alan/pane")
        });

        assert!(result.is_err());

        // lock still released despite error
        let lock_dir = dir.path().join(".claude.json.lock");
        assert!(!lock_dir.exists());
    }

    #[test]
    fn skips_write_when_already_trusted() {
        let dir = TempDir::new().unwrap();
        let path = config_in(dir.path());

        let initial = r#"{"projects": {"/Users/alan/pane": {"hasTrustDialogAccepted": true}}}"#;
        fs::write(&path, initial).unwrap();
        let mtime_before = fs::metadata(&path).unwrap().modified().unwrap();

        // small sleep so mtime would differ if written
        std::thread::sleep(std::time::Duration::from_millis(10));

        modify_config(&path, |config| {
            ensure_trust(config, "/Users/alan/pane")
        })
        .unwrap();

        // file not rewritten — mtime unchanged
        let mtime_after = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after);

        // no backup created
        let backups: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| {
                let name = e.ok()?.file_name().into_string().ok()?;
                if name.starts_with(".claude.json.backup.") {
                    Some(name)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(backups.len(), 0);
    }

    #[test]
    fn full_round_trip_with_realistic_config() {
        let dir = TempDir::new().unwrap();
        let path = config_in(dir.path());

        // realistic sparse config matching what the CLI writes
        let initial = r#"{
  "numStartups": 812,
  "installMethod": "native",
  "autoUpdates": false,
  "verbose": true,
  "editorMode": "vim",
  "autoCompactEnabled": false,
  "projects": {
    "/Users/alan": {
      "hasTrustDialogAccepted": true,
      "allowedTools": ["Bash", "Read", "Write", "Edit"]
    },
    "/Users/alan/code": {
      "hasTrustDialogAccepted": true
    }
  }
}"#;

        fs::write(&path, initial).unwrap();

        modify_config(&path, |config| {
            ensure_trust(config, "/Users/alan/pane")
        })
        .unwrap();

        let content = fs::read_to_string(&path).unwrap();
        let parsed: Value = serde_json::from_str(&content).unwrap();

        // new entry present
        assert_eq!(
            parsed["projects"]["/Users/alan/pane"]["hasTrustDialogAccepted"],
            true
        );

        // existing entries untouched
        assert_eq!(
            parsed["projects"]["/Users/alan"]["hasTrustDialogAccepted"],
            true
        );
        assert_eq!(
            parsed["projects"]["/Users/alan"]["allowedTools"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        assert_eq!(
            parsed["projects"]["/Users/alan/code"]["hasTrustDialogAccepted"],
            true
        );

        // top-level untouched
        assert_eq!(parsed["numStartups"], 812);
        assert_eq!(parsed["editorMode"], "vim");
        assert_eq!(parsed["autoCompactEnabled"], false);
    }
}
