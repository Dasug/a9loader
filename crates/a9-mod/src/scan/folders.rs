//! The `folder`-form `[[asset]]` expansion — one section becomes one
//! single-file-form op per file discovered under it, recursive and sorted.

use crate::manifest::{AssetSection, Manifest};
use crate::ModError;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The trimmed, non-empty `folder` of a folder-form section, if any.
fn folder_name(a: &AssetSection) -> Option<String> {
    a.folder
        .clone()
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
}

/// The three renderings of a folder-form path: the mod-relative path with `/`
/// separators, its `\`-joined form (the row-`dir` fallback), and the directory
/// to walk.
fn folder_root(mod_dir: &Path, folder: &str) -> (String, String, PathBuf) {
    // mod-relative path with '/' separators; "." / "" = the mod root
    let raw = folder.replace('\\', "/");
    let raw = raw.trim_matches('/').to_string();
    let folder_path = if raw == "." { String::new() } else { raw };
    let folder_dir = folder_path.replace('/', "\\");
    let root = if folder_path.is_empty() {
        mod_dir.to_path_buf()
    } else {
        mod_dir.join(&folder_path)
    };
    (folder_path, folder_dir, root)
}

/// Expand every `folder`-form `[[asset]]` section into one single-file-form op
/// per file discovered under it (recursive, sorted = deterministic):
///
/// - the file's **basename** becomes the row `name`;
/// - the file's **subfolder path relative to the folder** (with `\`
///   separators) becomes the row `dir` (e.g. `assets/train/mytrain/_t_x.x` for
///   `folder = "assets"` → dir `train\mytrain`);
/// - a file directly in the folder has no subfolder, so it takes the folder's
///   own path as its `dir`; when the folder is the mod root (`folder = "."`)
///   that path is empty, so the section's explicit `dir` is used instead;
/// - `host` (possibly `"auto"`) and `override_vanilla` carry over unchanged;
/// - two files with the same basename anywhere in one folder are a HARD ERROR
///   (bare i.dat names must be globally unique).
///
/// The expanded `file` uses `/` separators so `Path::join` works on both the
/// authoring host and the game (Windows accepts `/`).
pub fn expand_asset_folders(m: &mut Manifest, mod_dir: &Path) -> Result<(), ModError> {
    if !m.asset.iter().any(|a| a.folder.is_some()) {
        return Ok(());
    }
    let mut out: Vec<AssetSection> = Vec::with_capacity(m.asset.len());
    for a in std::mem::take(&mut m.asset) {
        match folder_name(&a) {
            Some(folder) => out.extend(expand_one_folder(&a, &folder, mod_dir)?),
            None => out.push(a),
        }
    }
    m.asset = out;
    Ok(())
}

/// Expand one folder-form section into one single-file-form op per file under
/// it, in sorted (deterministic) order.
fn expand_one_folder(
    a: &AssetSection,
    folder: &str,
    mod_dir: &Path,
) -> Result<Vec<AssetSection>, ModError> {
    let (folder_path, folder_dir, root) = folder_root(mod_dir, folder);
    if !root.is_dir() {
        return Err(ModError::Io(format!(
            "asset folder {folder:?}: {} is not a directory",
            root.display()
        )));
    }
    let mut files = Vec::new();
    collect_files(&root, &root, &mut files)?;
    files.sort();
    if files.is_empty() {
        return Err(ModError::Invalid(format!(
            "asset folder {folder:?}: no files under {}",
            root.display()
        )));
    }
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    let mut out = Vec::with_capacity(files.len());
    for rel in files {
        let name = rel.rsplit('/').next().unwrap_or(&rel).to_string();
        let parent = match rel.rsplit_once('/') {
            Some((p, _)) => p.replace('/', "\\"),
            None => String::new(),
        };
        let dir = if !parent.is_empty() {
            parent
        } else if !folder_dir.is_empty() {
            folder_dir.clone()
        } else {
            a.dir.clone()
        };
        if let Some(prev) = seen.insert(name.clone(), rel.clone()) {
            return Err(ModError::Invalid(format!(
                "asset folder {folder:?}: two files named {name:?} ({prev} and {rel}) — \
                 bare i.dat names must be globally unique"
            )));
        }
        let file = if folder_path.is_empty() {
            rel.clone()
        } else {
            format!("{folder_path}/{rel}")
        };
        out.push(AssetSection {
            name,
            dir,
            host: a.host.clone(),
            file,
            folder: None,
            override_vanilla: a.override_vanilla,
        });
    }
    Ok(out)
}

/// Recursively collect regular files under `dir` as `/`-joined paths relative
/// to `base` (symlinks are followed; unreadable entries are an error).
fn collect_files(base: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), ModError> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| ModError::Io(format!("reading {}: {e}", dir.display())))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            collect_files(base, &p, out)?;
        } else {
            let rel = p
                .strip_prefix(base)
                .map_err(|e| ModError::Io(format!("{}: {e}", p.display())))?;
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}
