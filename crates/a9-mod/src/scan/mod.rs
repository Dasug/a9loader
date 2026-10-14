//! Mods-dir scanning — every `<mods_dir>/<mod>/mod.toml`, parsed and
//! validated, sorted deterministically by directory name. Parse/validation
//! failures produce a per-mod error entry (the mod is skipped loudly; other
//! mods still load).
//!
//! Manifests carrying `[native]` (mods that ship code — post-M4) are refused
//! with a milestone pointer — refusing beats half-executing a design the
//! current loader cannot honor.
//!
//! The `folder`-form `[[asset]]` expansion lives in [`folders`].

use crate::manifest::{CsvMode, Manifest};
use crate::ModError;
use std::path::{Path, PathBuf};

mod folders;
pub use folders::expand_asset_folders;

#[derive(Debug, Clone)]
pub struct ScannedMod {
    /// the mod directory (manifest's base for relative paths)
    pub dir: PathBuf,
    pub manifest: Result<Manifest, ModError>,
}

pub fn scan(mods_dir: &Path) -> Vec<ScannedMod> {
    let Ok(entries) = std::fs::read_dir(mods_dir) else {
        return Vec::new(); // no mods dir = no mods; the loader logs this
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        // the loader's own overlay cache dir is not a mod — never scanned
        .filter(|p| p.file_name().map(|n| n != "cache").unwrap_or(true))
        .collect();
    dirs.sort();
    let mut out = Vec::new();
    for dir in dirs {
        let manifest_path = dir.join("mod.toml");
        let manifest = std::fs::read_to_string(&manifest_path)
            .map_err(|e| ModError::Io(format!("mod.toml: {e}")))
            .and_then(|text| parse_manifest(&text))
            .and_then(|mut m| {
                m.validate()?;
                // expand `folder`-form [[asset]] sections into ordinary
                // single-file-form ops so resolution/overlap/merge see only
                // per-name ops (M5 W1b); then re-validate to catch duplicate
                // basenames across the whole resolved set (bare i.dat names
                // are globally unique per build)
                expand_asset_folders(&mut m, &dir)?;
                m.validate()?;
                prime_csv_row_keys(&mut m, &dir);
                Ok(m)
            });
        out.push(ScannedMod { dir, manifest });
    }
    out
}

/// Fill each `mode = "rows"` [[csv]] section's `row_keys` from its patch file
/// so the resolver can key per-row overlaps (`csv:<table>:<rowkey>`). Best
/// effort: a file that cannot be read/parsed leaves the keys empty (the merge
/// then refuses loudly at serve time; the resolver falls back to a
/// whole-table key, which is fail-safe rather than silently permissive).
fn prime_csv_row_keys(m: &mut Manifest, dir: &Path) {
    for c in &mut m.csv {
        if c.mode == CsvMode::Rows {
            if let Ok(bytes) = std::fs::read(dir.join(&c.file)) {
                if let Ok(table) = a9_formats::csv::parse_patch(&bytes) {
                    c.row_keys = table.patch_keys();
                }
            }
        }
    }
}

fn parse_manifest(text: &str) -> Result<Manifest, ModError> {
    match toml::from_str::<Manifest>(text) {
        Ok(m) => Ok(m),
        Err(e) => {
            let msg = e.to_string();
            // friendly pointer for designed-but-not-yet-supported sections
            if msg.contains("unknown field `native`") {
                return Err(ModError::Parse(
                    "section [native] is designed but lands with M4+ (mods that ship code); \
                     this loader build does not support it yet"
                        .into(),
                ));
            }
            Err(ModError::Parse(msg))
        }
    }
}

#[cfg(test)]
mod tests;
