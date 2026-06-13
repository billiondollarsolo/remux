//! Library-side view of the `cmd` tree: surfaces only `fleet` for integration
//! tests (the binary's `cmd/mod.rs` declares the full set). Kept separate so the
//! lib crate doesn't pull in daemon-attaching command modules it doesn't need.
pub mod fleet;
