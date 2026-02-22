// Session ID tracking. Each pane window gets a JSONL file at
// ~/.local/state/puzzle/<slug>/sessions.jsonl. The most recent line is the
// current session. Append to record a new session, read the last line to
// resume.
//
// The slug is the directory name under ~/pane/ — "puzzle" for ~/pane/puzzle.
// Each slug gets its own directory alongside its windows/ subdirectory.
//
// The format is one JSON object per line with at minimum a session_id field.
// A timestamp is included for human readability when tailing the file by hand.
// No locking — only one Puzzle instance writes to a given slug's file at a time.

use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionEntry {
    pub session_id: String,
    pub timestamp: String,
}

/// Build the path to the sessions file for a slug.
/// ~/.local/state/puzzle/<slug>/sessions.jsonl
pub fn sessions_path(config_dir: &Path, slug: &str) -> PathBuf {
    config_dir.join(slug).join("sessions.jsonl")
}

/// Derive the slug from a pane directory path. Strips the ~/pane/ prefix and
/// returns the remaining path component. For ~/pane/puzzle, returns "puzzle".
pub fn slug_from_pane_dir(pane_dir: &Path) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let pane_root = PathBuf::from(&home).join("pane");
    let stripped = pane_dir.strip_prefix(&pane_root).ok()?;
    Some(stripped.to_str()?.to_string())
}

/// Read the most recent session ID for a slug. Returns None if the file
/// doesn't exist or is empty.
pub fn latest_session(config_dir: &Path, slug: &str) -> Option<String> {
    let path = sessions_path(config_dir, slug);
    let content = fs::read_to_string(&path).ok()?;
    let last_line = content.lines().rev().find(|l| !l.trim().is_empty())?;
    let entry: SessionEntry = serde_json::from_str(last_line).ok()?;
    Some(entry.session_id)
}

/// Append a session ID for a slug. Creates the directory and file if needed.
pub fn record_session(
    config_dir: &Path,
    slug: &str,
    session_id: &str,
) -> Result<(), std::io::Error> {
    let path = sessions_path(config_dir, slug);
    fs::create_dir_all(path.parent().unwrap())?;

    let entry = SessionEntry {
        session_id: session_id.to_string(),
        timestamp: now_iso8601(),
    };

    let mut line = serde_json::to_string(&entry)
        .expect("SessionEntry serialization cannot fail");
    line.push('\n');

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(line.as_bytes())?;

    Ok(())
}

/// Read all session IDs for a slug, most recent last.
pub fn all_sessions(config_dir: &Path, slug: &str) -> Vec<SessionEntry> {
    let path = sessions_path(config_dir, slug);
    let file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    std::io::BufReader::new(file)
        .lines()
        .filter_map(|line| {
            let line = line.ok()?;
            serde_json::from_str(&line).ok()
        })
        .collect()
}

fn now_iso8601() -> String {
    // chrono would be nice but we don't need another dependency for a timestamp.
    // shell out to date? no. just use SystemTime and format manually.
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let secs = dur.as_secs();

    // good enough for a human-readable timestamp in a jsonl file.
    // not worth adding chrono for this.
    format!("{}", secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn record_and_read_latest() {
        let dir = TempDir::new().unwrap();

        record_session(dir.path(), "puzzle", "abc-123").unwrap();
        record_session(dir.path(), "puzzle", "def-456").unwrap();

        let latest = latest_session(dir.path(), "puzzle").unwrap();
        assert_eq!(latest, "def-456");
    }

    #[test]
    fn latest_returns_none_for_missing_slug() {
        let dir = TempDir::new().unwrap();
        assert_eq!(latest_session(dir.path(), "nonexistent"), None);
    }

    #[test]
    fn latest_returns_none_for_empty_file() {
        let dir = TempDir::new().unwrap();
        let path = sessions_path(dir.path(), "puzzle");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "").unwrap();

        assert_eq!(latest_session(dir.path(), "puzzle"), None);
    }

    #[test]
    fn all_sessions_returns_entries_in_order() {
        let dir = TempDir::new().unwrap();

        record_session(dir.path(), "puzzle", "first").unwrap();
        record_session(dir.path(), "puzzle", "second").unwrap();
        record_session(dir.path(), "puzzle", "third").unwrap();

        let entries = all_sessions(dir.path(), "puzzle");
        let ids: Vec<&str> = entries.iter().map(|e| e.session_id.as_str()).collect();
        assert_eq!(ids, vec!["first", "second", "third"]);
    }

    #[test]
    fn separate_slugs_are_independent() {
        let dir = TempDir::new().unwrap();

        record_session(dir.path(), "puzzle", "puzzle-session").unwrap();
        record_session(dir.path(), "dotfiles", "dotfiles-session").unwrap();

        assert_eq!(
            latest_session(dir.path(), "puzzle").unwrap(),
            "puzzle-session"
        );
        assert_eq!(
            latest_session(dir.path(), "dotfiles").unwrap(),
            "dotfiles-session"
        );
    }

    #[test]
    fn creates_directory_structure() {
        let dir = TempDir::new().unwrap();

        record_session(dir.path(), "puzzle", "abc-123").unwrap();

        assert!(dir.path().join("puzzle").is_dir());
        assert!(dir.path().join("puzzle").join("sessions.jsonl").is_file());
    }

    #[test]
    fn file_is_valid_jsonl() {
        let dir = TempDir::new().unwrap();

        record_session(dir.path(), "puzzle", "abc-123").unwrap();
        record_session(dir.path(), "puzzle", "def-456").unwrap();

        let content = fs::read_to_string(sessions_path(dir.path(), "puzzle")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);

        // each line parses independently
        for line in &lines {
            let entry: SessionEntry = serde_json::from_str(line).unwrap();
            assert!(!entry.session_id.is_empty());
            assert!(!entry.timestamp.is_empty());
        }
    }

    #[test]
    fn sessions_path_structure() {
        let state = Path::new("/home/alan/.local/state/puzzle");
        let path = sessions_path(state, "puzzle");
        assert_eq!(
            path,
            PathBuf::from("/home/alan/.local/state/puzzle/puzzle/sessions.jsonl")
        );
    }

    #[test]
    fn slug_from_pane_dir_extracts_name() {
        // this test depends on HOME being set, which it is in any real env
        if let Ok(home) = std::env::var("HOME") {
            let pane_dir = PathBuf::from(&home).join("pane").join("puzzle");
            let slug = slug_from_pane_dir(&pane_dir).unwrap();
            assert_eq!(slug, "puzzle");
        }
    }

    #[test]
    fn tolerates_malformed_lines() {
        let dir = TempDir::new().unwrap();
        let path = sessions_path(dir.path(), "puzzle");
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        // write some valid and some garbage lines
        fs::write(
            &path,
            "{\"session_id\":\"good-one\",\"timestamp\":\"123\"}\ngarbage\n{\"session_id\":\"good-two\",\"timestamp\":\"456\"}\n",
        )
        .unwrap();

        // latest should be the last valid line
        let latest = latest_session(dir.path(), "puzzle").unwrap();
        assert_eq!(latest, "good-two");

        // all_sessions skips the garbage
        let entries = all_sessions(dir.path(), "puzzle");
        assert_eq!(entries.len(), 2);
    }
}
