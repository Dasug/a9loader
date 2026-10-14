//! Subcommand implementations, one module per command family.
//!
//! `main.rs` owns the clap argument model and dispatches here; each module
//! below holds the entry point (and any helpers private to it). The offline
//! `list`/`doctor` report lives in [`report`], split into one small function
//! per printed section so a change to one section stays local.

pub mod csv;
pub mod diff;
pub mod inspect;
pub mod list;
pub mod mkpack;
pub mod report;
pub mod selftests;
pub mod verify;
