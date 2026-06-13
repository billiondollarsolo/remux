use std::path::PathBuf;

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use remux_core::{RemuxError, TermSize};

/// A spawned PTY process with its master file descriptor and child handle.
pub struct PtyProcess {
    pub pid: u32,
    pub master: Box<dyn std::io::Read + Send + 'static>,
    pub writer: Box<dyn std::io::Write + Send + 'static>,
    pub child: Box<dyn portable_pty::Child + Send + 'static>,
    pub master_pty: Box<dyn portable_pty::MasterPty + Send + 'static>,
}

/// Spawn a new PTY with the given command, working directory, environment, and terminal size.
pub fn spawn_pty(
    command: Vec<String>,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    size: TermSize,
) -> Result<PtyProcess, RemuxError> {
    let pty_system = native_pty_system();

    let pair = pty_system
        .openpty(PtySize {
            rows: size.rows,
            cols: size.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| RemuxError::PtyError(format!("failed to open pty: {e}")))?;

    let cmd = build_command(&command, &cwd, &env);

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| RemuxError::PtyError(format!("failed to spawn command: {e}")))?;

    let pid = match child.process_id() {
        Some(pid) => pid,
        None => {
            return Err(RemuxError::PtyError(
                "failed to get child process id".to_string(),
            ))
        }
    };

    let master = pair
        .master
        .try_clone_reader()
        .map_err(|e| RemuxError::PtyError(format!("failed to clone pty reader: {e}")))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| RemuxError::PtyError(format!("failed to get pty writer: {e}")))?;

    Ok(PtyProcess {
        pid,
        master,
        writer,
        child,
        master_pty: pair.master,
    })
}

/// Build a CommandBuilder from the given command args, cwd, and env.
/// If command is empty, falls back to $SHELL (or /bin/sh).
fn build_command(command: &[String], cwd: &PathBuf, env: &[(String, String)]) -> CommandBuilder {
    let (cmd_name, args) = if command.is_empty() {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        (shell, vec![])
    } else {
        let mut parts = command.iter();
        let name = parts
            .next()
            .expect("command should have at least one element")
            .clone();
        let rest: Vec<String> = parts.cloned().collect();
        (name, rest)
    };

    let mut cmd = CommandBuilder::new(cmd_name);
    cmd.args(args);
    cmd.cwd(cwd);
    cmd.env("TERM", "xterm-256color");

    for (key, value) in env {
        cmd.env(key.as_str(), value.as_str());
    }

    cmd
}

/// Resize a PTY using a MasterPty trait object (portable approach).
pub fn resize_pty_master(master: &dyn MasterPty, size: TermSize) -> Result<(), RemuxError> {
    master
        .resize(PtySize {
            rows: size.rows,
            cols: size.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| RemuxError::PtyError(format!("failed to resize pty: {e}")))?;
    Ok(())
}
