use serde::{Deserialize, Serialize};

use crate::error::RemuxError;

/// Top-level configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default)]
    pub data: DataConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub client: ClientConfig,
}

impl Config {
    /// Load configuration from a TOML file at the given path.
    pub fn load(path: &std::path::Path) -> Result<Self, RemuxError> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| RemuxError::ConfigError(format!("failed to read config file: {e}")))?;
        let config: Config = toml::from_str(&contents)
            .map_err(|e| RemuxError::ConfigError(format!("failed to parse config: {e}")))?;
        Ok(config)
    }

    /// Load configuration from a TOML string (useful for testing).
    pub fn from_toml_str(s: &str) -> Result<Self, RemuxError> {
        let config: Config = toml::from_str(s)
            .map_err(|e| RemuxError::ConfigError(format!("failed to parse config: {e}")))?;
        Ok(config)
    }
}

/// Data storage configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DataConfig {
    #[serde(default = "default_data_dir")]
    pub dir: std::path::PathBuf,
}

fn default_data_dir() -> std::path::PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("remux")
}

impl Default for DataConfig {
    fn default() -> Self {
        Self {
            dir: default_data_dir(),
        }
    }
}

/// Daemon configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DaemonConfig {
    #[serde(default = "default_socket_path")]
    pub socket_path: std::path::PathBuf,
    #[serde(default = "default_scrollback")]
    pub max_scrollback_lines: usize,
    #[serde(default)]
    pub persist_scrollback: bool,
    #[serde(default = "default_cleanup_hours")]
    pub cleanup_exited_after_hours: u64,
}

fn default_socket_path() -> std::path::PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("remux")
        .join("remuxd.sock")
}

fn default_scrollback() -> usize {
    20_000
}

fn default_cleanup_hours() -> u64 {
    168
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket_path: default_socket_path(),
            max_scrollback_lines: default_scrollback(),
            persist_scrollback: false,
            cleanup_exited_after_hours: default_cleanup_hours(),
        }
    }
}

/// Client configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientConfig {
    #[serde(default = "default_shell")]
    pub default_shell: String,
    #[serde(default = "default_detach_key")]
    pub detach_key: String,
    /// Show a persistent status line at the bottom of the screen during a live
    /// attach (tmux-style). Disable per-attach with `--no-status`.
    #[serde(default = "default_status_line")]
    pub status_line: bool,
}

fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

fn default_detach_key() -> String {
    "ctrl-a".to_string()
}

fn default_status_line() -> bool {
    true
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            default_shell: default_shell(),
            detach_key: default_detach_key(),
            status_line: default_status_line(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_values() {
        let config = Config::default();
        assert_eq!(config.data.dir, dirs::data_dir().unwrap().join("remux"));
        assert_eq!(
            config.daemon.socket_path,
            dirs::state_dir().unwrap().join("remux").join("remuxd.sock")
        );
        assert_eq!(config.daemon.max_scrollback_lines, 20_000);
        assert!(!config.daemon.persist_scrollback);
        assert_eq!(config.daemon.cleanup_exited_after_hours, 168);
        assert_eq!(config.client.detach_key, "ctrl-a");
        assert!(config.client.status_line);
    }

    #[test]
    fn config_from_toml_string() {
        let toml = r#"
[daemon]
max_scrollback_lines = 5000
persist_scrollback = true

[client]
default_shell = "/bin/zsh"
detach_key = "ctrl-a"
status_line = false
"#;
        let config = Config::from_toml_str(toml).expect("parse");
        assert_eq!(config.daemon.max_scrollback_lines, 5000);
        assert!(config.daemon.persist_scrollback);
        assert_eq!(config.client.default_shell, "/bin/zsh");
        assert_eq!(config.client.detach_key, "ctrl-a");
        assert!(!config.client.status_line);
        // Data section should retain defaults since not specified
        assert_eq!(config.data.dir, dirs::data_dir().unwrap().join("remux"));
    }

    #[test]
    fn config_empty_toml_uses_defaults() {
        let config = Config::from_toml_str("").expect("parse empty");
        assert_eq!(config.daemon.max_scrollback_lines, 20_000);
        assert_eq!(config.client.detach_key, "ctrl-a");
        assert!(config.client.status_line);
    }

    #[test]
    fn config_roundtrip_toml() {
        let config = Config::default();
        let toml_str = toml::to_string(&config).expect("serialize to toml");
        let back: Config = toml::from_str(&toml_str).expect("deserialize from toml");
        assert_eq!(config, back);
    }
}
