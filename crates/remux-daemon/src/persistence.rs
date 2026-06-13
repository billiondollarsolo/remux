use std::path::PathBuf;

use remux_core::{Config, RemuxError, SessionId};

/// Metadata persisted for each session to disk.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PersistedSession {
    pub id: SessionId,
    pub name: String,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Save session metadata as JSON to the data directory.
pub fn save_session(config: &Config, session: &PersistedSession) -> Result<(), RemuxError> {
    let sessions_dir = config.data.dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir)
        .map_err(|e| RemuxError::IoError(format!("failed to create sessions dir: {e}")))?;

    let path = sessions_dir.join(format!("{}.json", session.id.0));
    let json = serde_json::to_string_pretty(session)
        .map_err(|e| RemuxError::IoError(format!("failed to serialize session: {e}")))?;

    std::fs::write(&path, json)
        .map_err(|e| RemuxError::IoError(format!("failed to write session file: {e}")))?;

    tracing::debug!(session_id = %session.id.0, path = %path.display(), "saved session metadata");
    Ok(())
}

/// Load all persisted sessions from the data directory.
pub fn load_sessions(config: &Config) -> Vec<PersistedSession> {
    let sessions_dir = config.data.dir.join("sessions");
    if !sessions_dir.exists() {
        return Vec::new();
    }

    let mut sessions = Vec::new();
    let entries = match std::fs::read_dir(&sessions_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(path = %sessions_dir.display(), error = %e, "failed to read sessions dir");
            return Vec::new();
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }

        match std::fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str::<PersistedSession>(&contents) {
                Ok(session) => sessions.push(session),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "failed to parse session file");
                }
            },
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to read session file");
            }
        }
    }

    tracing::info!(count = sessions.len(), "loaded persisted sessions");
    sessions
}

/// Remove a session's persisted metadata file (and its scrollback file).
pub fn remove_session(config: &Config, id: &SessionId) -> Result<(), RemuxError> {
    let sessions_dir = config.data.dir.join("sessions");

    let path = sessions_dir.join(format!("{}.json", id.0));
    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|e| RemuxError::IoError(format!("failed to remove session file: {e}")))?;
        tracing::debug!(session_id = %id.0, "removed session metadata file");
    }

    // Best-effort removal of the companion scrollback file. A missing file is
    // not an error (the session may not have had scrollback persistence on).
    let scrollback_path = sessions_dir.join(format!("{}.scrollback", id.0));
    if scrollback_path.exists() {
        std::fs::remove_file(&scrollback_path)
            .map_err(|e| RemuxError::IoError(format!("failed to remove scrollback file: {e}")))?;
        tracing::debug!(session_id = %id.0, "removed session scrollback file");
    }

    Ok(())
}

/// Save a session's scrollback to disk as newline-delimited raw bytes.
///
/// Written to `<data.dir>/sessions/<id>.scrollback`. The number of lines is
/// capped at `max_scrollback_lines` (keeping the most recent lines) so the file
/// never grows beyond the configured retention. Each line is stored verbatim
/// followed by a single `\n` separator; since scrollback lines have already had
/// their own trailing newlines stripped by `ScrollbackBuffer`, this is a clean
/// newline-delimited encoding.
pub fn save_scrollback(
    config: &Config,
    id: &SessionId,
    lines: &[Vec<u8>],
) -> Result<(), RemuxError> {
    let sessions_dir = config.data.dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir)
        .map_err(|e| RemuxError::IoError(format!("failed to create sessions dir: {e}")))?;

    let path = sessions_dir.join(format!("{}.scrollback", id.0));

    // Cap to the most recent `max_scrollback_lines` lines.
    let max = config.daemon.max_scrollback_lines;
    let start = lines.len().saturating_sub(max);
    let kept = &lines[start..];

    let mut buf = Vec::new();
    for line in kept {
        buf.extend_from_slice(line);
        buf.push(b'\n');
    }

    std::fs::write(&path, &buf)
        .map_err(|e| RemuxError::IoError(format!("failed to write scrollback file: {e}")))?;

    tracing::trace!(session_id = %id.0, lines = kept.len(), "saved scrollback");
    Ok(())
}

/// Load a session's scrollback from disk, split on newline boundaries.
///
/// Returns an empty vector if the file is missing or unreadable (recovery is
/// best-effort: a missing scrollback file simply means there is no history to
/// restore). A trailing empty line from the final separator is not emitted.
pub fn load_scrollback(config: &Config, id: &SessionId) -> Vec<Vec<u8>> {
    let path = config
        .data
        .dir
        .join("sessions")
        .join(format!("{}.scrollback", id.0));

    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %path.display(), error = %e, "failed to read scrollback file");
            }
            return Vec::new();
        }
    };

    let mut lines: Vec<Vec<u8>> = bytes.split(|&b| b == b'\n').map(|s| s.to_vec()).collect();
    // `split` yields a trailing empty element after the final separator; drop it.
    if lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines
}

/// Prune persisted session files whose `created_at` is older than
/// `cleanup_exited_after_hours`.
///
/// Semantics of `cleanup_exited_after_hours == 0`: treated as **disabled**
/// (cleanup is skipped entirely). `0` is ambiguous between "never" and
/// "immediate"; treating it as "disabled/never" is the safe choice so a
/// misconfigured `0` cannot wipe all recovered sessions on startup.
pub fn cleanup_old_sessions(config: &Config) {
    let hours = config.daemon.cleanup_exited_after_hours;
    if hours == 0 {
        tracing::debug!("cleanup_exited_after_hours == 0; skipping session cleanup");
        return;
    }

    let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours as i64);
    let mut pruned = 0usize;
    for session in load_sessions(config) {
        if session.created_at < cutoff {
            if let Err(e) = remove_session(config, &session.id) {
                tracing::warn!(session_id = %session.id.0, error = %e, "failed to prune old session");
            } else {
                pruned += 1;
            }
        }
    }

    if pruned > 0 {
        tracing::info!(pruned, hours, "pruned old persisted sessions");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remux_core::SessionId;

    #[test]
    fn persisted_session_serializes() {
        let session = PersistedSession {
            id: SessionId::new(),
            name: "test".to_string(),
            command: vec!["bash".to_string()],
            cwd: PathBuf::from("/tmp"),
            created_at: chrono::Utc::now(),
        };

        let json = serde_json::to_string(&session).unwrap();
        let back: PersistedSession = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "test");
        assert_eq!(back.command, vec!["bash"]);
    }

    #[test]
    fn save_and_load_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            data: remux_core::config::DataConfig {
                dir: dir.path().to_path_buf(),
            },
            ..Config::default()
        };

        let id = SessionId::new();
        let session = PersistedSession {
            id: id.clone(),
            name: "test-session".to_string(),
            command: vec!["zsh".to_string()],
            cwd: PathBuf::from("/home/user"),
            created_at: chrono::Utc::now(),
        };

        save_session(&config, &session).unwrap();

        let loaded = load_sessions(&config);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "test-session");
        assert_eq!(loaded[0].id, id);

        remove_session(&config, &id).unwrap();
        let loaded_after_remove = load_sessions(&config);
        assert!(loaded_after_remove.is_empty());
    }

    fn config_with_dir(dir: &std::path::Path) -> Config {
        Config {
            data: remux_core::config::DataConfig {
                dir: dir.to_path_buf(),
            },
            ..Config::default()
        }
    }

    #[test]
    fn scrollback_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_dir(dir.path());
        let id = SessionId::new();

        let lines: Vec<Vec<u8>> = vec![
            b"first line".to_vec(),
            b"second line".to_vec(),
            // Embedded non-newline binary bytes must round-trip verbatim.
            vec![0x1b, b'[', b'3', b'1', b'm', b'r', b'e', b'd'],
            b"".to_vec(), // an empty (blank) line in the middle
            b"last line".to_vec(),
        ];

        save_scrollback(&config, &id, &lines).unwrap();
        let loaded = load_scrollback(&config, &id);
        assert_eq!(loaded, lines);
    }

    #[test]
    fn scrollback_missing_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_dir(dir.path());
        let id = SessionId::new();
        assert!(load_scrollback(&config, &id).is_empty());
    }

    #[test]
    fn scrollback_caps_at_max_lines() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_dir(dir.path());
        config.daemon.max_scrollback_lines = 2;
        let id = SessionId::new();

        let lines: Vec<Vec<u8>> = vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()];
        save_scrollback(&config, &id, &lines).unwrap();
        let loaded = load_scrollback(&config, &id);
        // Only the two most recent lines are kept.
        assert_eq!(loaded, vec![b"c".to_vec(), b"d".to_vec()]);
    }

    #[test]
    fn remove_session_deletes_scrollback_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_dir(dir.path());
        let id = SessionId::new();

        save_session(
            &config,
            &PersistedSession {
                id: id.clone(),
                name: "s".to_string(),
                command: vec!["bash".to_string()],
                cwd: PathBuf::from("/tmp"),
                created_at: chrono::Utc::now(),
            },
        )
        .unwrap();
        save_scrollback(&config, &id, &[b"line".to_vec()]).unwrap();

        let scrollback_path = dir
            .path()
            .join("sessions")
            .join(format!("{}.scrollback", id.0));
        assert!(scrollback_path.exists());

        remove_session(&config, &id).unwrap();
        assert!(!scrollback_path.exists());
        assert!(load_scrollback(&config, &id).is_empty());
    }

    #[test]
    fn cleanup_prunes_old_sessions_and_keeps_recent() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_dir(dir.path());
        config.daemon.cleanup_exited_after_hours = 24;

        let old_id = SessionId::new();
        let new_id = SessionId::new();

        save_session(
            &config,
            &PersistedSession {
                id: old_id.clone(),
                name: "old".to_string(),
                command: vec!["bash".to_string()],
                cwd: PathBuf::from("/tmp"),
                created_at: chrono::Utc::now() - chrono::Duration::hours(48),
            },
        )
        .unwrap();
        save_session(
            &config,
            &PersistedSession {
                id: new_id.clone(),
                name: "new".to_string(),
                command: vec!["bash".to_string()],
                cwd: PathBuf::from("/tmp"),
                created_at: chrono::Utc::now(),
            },
        )
        .unwrap();

        cleanup_old_sessions(&config);

        let remaining = load_sessions(&config);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, new_id);
    }

    #[test]
    fn cleanup_zero_hours_is_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_dir(dir.path());
        config.daemon.cleanup_exited_after_hours = 0;

        let id = SessionId::new();
        save_session(
            &config,
            &PersistedSession {
                id: id.clone(),
                name: "ancient".to_string(),
                command: vec!["bash".to_string()],
                cwd: PathBuf::from("/tmp"),
                created_at: chrono::Utc::now() - chrono::Duration::hours(10_000),
            },
        )
        .unwrap();

        cleanup_old_sessions(&config);

        // With 0 == disabled, even an ancient session survives.
        assert_eq!(load_sessions(&config).len(), 1);
    }
}
