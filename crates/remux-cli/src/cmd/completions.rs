//! `remux completions <shell>` — generate shell completion scripts.

use std::io;

use clap::CommandFactory;
use clap_complete::{generate, Shell};
use remux_core::RemuxError;

use crate::Cli;

/// Generate a completion script for `shell` and write it to stdout.
pub fn run(shell: Shell) -> Result<(), RemuxError> {
    let mut cmd = Cli::command();
    let bin_name = cmd.get_name().to_string();
    generate(shell, &mut cmd, bin_name, &mut io::stdout());
    Ok(())
}
