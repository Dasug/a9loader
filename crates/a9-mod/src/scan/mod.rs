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

use crate::manifest::{CsvMode, Manifest, TrainCsv};
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
            .and_then(|text| Manifest::parse(&text))
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
                read_train_csv(&mut m, &dir)?;
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
/// Read a mod's own train CSVs when it ships them. The files live in `trains/`
/// in the mod directory — a folder of its own, deliberately NOT `csv/`, which a
/// mod may already use for its `[[csv]]` patch files (the shipped
/// `new-train-template` ships `csv/train_form.csv` as exactly such a patch) —
/// and use the SHIPPED layouts verbatim — the same names, the
/// same column order — so a mod author copies a vanilla row and edits it. Rows
/// are keyed by the FORM name in column 0, exactly like the shipped tables; the
/// compiler matches them against each `[[train]]`'s allocated FORM name.
///
/// A file that exists but cannot be parsed refuses the mod loudly (its other
/// trains would silently lose the data the author wrote), matching how a broken
/// `[[asset]]` folder is treated.
pub fn read_train_csv_public(m: &mut Manifest, dir: &Path) -> Result<(), ModError> {
    read_train_csv(m, dir)
}

fn read_train_csv(m: &mut Manifest, dir: &Path) -> Result<(), ModError> {
    const TABLES: [&str; 4] = [
        "train_form.csv",
        "train_pattern.csv",
        "train_comment.csv",
        "train_custom.csv",
    ];
    let root = dir.join("trains");
    let mut set = TrainCsv::default();
    let mut any = false;
    for name in TABLES {
        let path = root.join(name);
        let Ok(bytes) = std::fs::read(&path) else {
            continue; // absent: the derived defaults stand
        };
        let table = a9_formats::csv::Table::parse(&bytes).map_err(|e| {
            ModError::Invalid(format!(
                "trains/{name} could not be parsed ({e}) — a train table must use the \
                 shipped layout verbatim (see the vanilla table of the same name)"
            ))
        })?;
        any = true;
        match name {
            "train_form.csv" => set.form = Some(table),
            "train_pattern.csv" => set.pattern = Some(table),
            "train_comment.csv" => set.comment = Some(table),
            _ => set.custom = Some(table),
        }
    }
    if any {
        m.train_csv = set;
    }
    Ok(())
}

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

#[cfg(test)]
mod tests;
