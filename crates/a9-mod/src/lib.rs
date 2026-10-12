//! a9-mod — the shared mod model: manifest schema, dependency resolution,
//! build identity (PE VERSIONINFO), the per-build anchor address book, the
//! loader config schema, and mods-dir scanning.
//!
//! Pure data + `std::fs` only — no Win32 — so the loader (in-game) and `a9ml`
//! (host) share the exact same behavior. The loader's engine consumes
//! [`manifest::Manifest`] + [`resolve::Resolution`] and executes the plan;
//! `a9ml list/doctor` reproduce the same resolution offline.

pub mod buildid;
pub mod builds;
pub mod config;
pub mod hex;
pub mod manifest;
pub mod resolve;
pub mod scan;

#[derive(Debug, Clone, thiserror::Error)]
pub enum ModError {
    #[error("io: {0}")]
    Io(String),
    #[error("parse: {0}")]
    Parse(String),
    #[error("invalid: {0}")]
    Invalid(String),
}
