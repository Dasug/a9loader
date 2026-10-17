//! Subcommand implementations, one module per command family.
//!
//! `main.rs` owns the clap argument model and dispatches here; each module
//! below holds the entry point (and any helpers private to it), including the
//! two data-model modules (`classify`, `asset`) and the two generators
//! (`gen_builds`, `gen_dirindex`). This is the whole rule: no implementation
//! lives at the crate root. The offline `list`/`doctor` report lives in
//! [`report`], split into one small function per printed section so a change
//! to one section stays local.

pub mod asset;
pub mod classify;
pub mod csv;
pub mod diff;
pub mod gen_builds;
pub mod gen_dirindex;
pub mod inspect;
pub mod list;
pub mod mkpack;
pub mod report;
pub mod selftests;
pub mod trains;
pub mod verify;
