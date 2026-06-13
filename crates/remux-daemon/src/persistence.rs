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
#[allow(dead_code)]
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

/// Remove a session's persisted metadata file.
#[allow(dead_code)]
pub fn remove_session(config: &Config, id: &SessionId) -> Result<(), RemuxError> {
    let path = config
        .data
        .dir
        .join("sessions")
        .join(format!("{}.json", id.0));
    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|e| RemuxError::IoError(format!("failed to remove session file: {e}")))?;
        tracing::debug!(session_id = %id.0, "removed session metadata file");
    }
    Ok(())
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
}
