//! Library surface of the `remux` CLI.
//!
//! The CLI is primarily a binary (`src/main.rs`), but the transport client is
//! also exposed here so integration tests can drive the *exact* production
//! transport (`RemuxClient::connect_via_command`) used by the SSH remote path,
//! without needing a real SSH server.
pub mod client;
