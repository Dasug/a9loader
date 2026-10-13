//! Mods-dir scanning — every `<mods_dir>/<mod>/mod.toml`, parsed and
//! validated, sorted deterministically by directory name. Parse/validation
//! failures produce a per-mod error entry (the mod is skipped loudly; other
//! mods still load).
//!
//! Manifests carrying `[native]` (mods that ship code — post-M4) are refused
//! with a milestone pointer — refusing beats half-executing a design the
//! current loader cannot honor.

use crate::manifest::{AssetSection, CsvMode, Manifest};
use crate::ModError;
use std::path::{Path, PathBuf};

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
        let folder = a
            .folder
            .clone()
            .map(|f| f.trim().to_string())
            .filter(|f| !f.is_empty());
        let Some(folder) = folder else {
            out.push(a);
            continue;
        };
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
        let mut seen: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
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
    }
    m.asset = out;
    Ok(())
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
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("a9mod-scan-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn scans_parses_and_sorts() {
        let dir = tmpdir("ok");
        std::fs::create_dir_all(dir.join("b-mod")).unwrap();
        std::fs::create_dir_all(dir.join("a-mod")).unwrap();
        std::fs::write(
            dir.join("a-mod/mod.toml"),
            "id=\"a\"\nname=\"A\"\nversion=\"1.0.0\"\nauthor=\"t\"\npriority=10\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("b-mod/mod.toml"),
            "id=\"b\"\nname=\"B\"\nversion=\"1.0.0\"\nauthor=\"t\"\n",
        )
        .unwrap();
        let scanned = scan(&dir);
        assert_eq!(scanned.len(), 2);
        assert!(scanned[0].dir.ends_with("a-mod"));
        assert_eq!(scanned[0].manifest.as_ref().unwrap().id, "a");
        assert_eq!(scanned[1].manifest.as_ref().unwrap().id, "b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_mods_dir_is_empty_silent() {
        assert!(scan(Path::new("/nonexistent-mods")).is_empty());
    }

    #[test]
    fn primes_row_keys_for_row_mode_csv_sections() {
        let dir = tmpdir("csvkeys");
        std::fs::create_dir_all(dir.join("m")).unwrap();
        std::fs::write(
            dir.join("m/mod.toml"),
            "id=\"m\"\nname=\"M\"\nversion=\"1.0.0\"\nauthor=\"t\"\n\
             [[csv]]\nname=\"train_comment.csv\"\nfile=\"p.csv\"\nmode=\"rows\"\n\
             [[csv]]\nname=\"train_form.csv\"\nfile=\"whole.csv\"\n",
        )
        .unwrap();
        // a UTF-16 patch file (BOM, CRLF, quoted)
        let mut patch = vec![0xff, 0xfe];
        for u in "\"FORM_1\",\"name\"\r\n\"FORM_2\",\"name2\"\r\n".encode_utf16() {
            patch.extend_from_slice(&u.to_le_bytes());
        }
        std::fs::write(dir.join("m/p.csv"), &patch).unwrap();
        std::fs::write(dir.join("whole.csv"), b"not parsed").unwrap();
        let scanned = scan(&dir);
        let m = scanned[0].manifest.as_ref().unwrap();
        assert_eq!(m.csv[0].row_keys, vec!["FORM_1".to_string(), "FORM_2".to_string()]);
        // replace mode is never primed
        assert!(m.csv[1].row_keys.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn primes_row_keys_from_a_utf8_patch_file() {
        // a plain UTF-8 patch file (what an editor writes) is accepted too
        let dir = tmpdir("csvkeys-utf8");
        std::fs::create_dir_all(dir.join("m")).unwrap();
        std::fs::write(
            dir.join("m/mod.toml"),
            "id=\"m\"\nname=\"M\"\nversion=\"1.0.0\"\nauthor=\"t\"\n\
             [[csv]]\nname=\"train_comment.csv\"\nfile=\"p.csv\"\nmode=\"rows\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("m/p.csv"), b"\"FORM_7\",\"x\"\r\n\"FORM_8\",\"y\"\r\n").unwrap();
        let scanned = scan(&dir);
        let m = scanned[0].manifest.as_ref().unwrap();
        assert_eq!(m.csv[0].row_keys, vec!["FORM_7".to_string(), "FORM_8".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_patch_file_leaves_keys_empty() {
        let dir = tmpdir("csvkeys-missing");
        std::fs::create_dir_all(dir.join("m")).unwrap();
        std::fs::write(
            dir.join("m/mod.toml"),
            "id=\"m\"\nname=\"M\"\nversion=\"1.0.0\"\nauthor=\"t\"\n\
             [[csv]]\nname=\"t.csv\"\nfile=\"gone.csv\"\nmode=\"rows\"\n",
        )
        .unwrap();
        let scanned = scan(&dir);
        assert!(scanned[0].manifest.as_ref().unwrap().csv[0].row_keys.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expands_asset_folder_recursive_deterministic_dirs() {
        let dir = tmpdir("assetfolder");
        std::fs::create_dir_all(dir.join("m/assets/train/mytrain/lod")).unwrap();
        std::fs::create_dir_all(dir.join("m/assets/train/other")).unwrap();
        std::fs::write(
            dir.join("m/mod.toml"),
            "id=\"m\"\nname=\"M\"\nversion=\"1.0.0\"\nauthor=\"t\"\n\
             [[asset]]\nfolder=\"assets\"\nhost=\"vp\"\n",
        )
        .unwrap();
        for (p, b) in [
            ("m/assets/train/mytrain/_t_a.x", b"a"),
            ("m/assets/train/mytrain/lod/_t_b.x", b"b"),
            ("m/assets/train/other/_t_c.att", b"c"),
            ("m/assets/x.dds", b"d"),
            ("m/assets/loose.dat", b"e"),
        ] {
            std::fs::write(dir.join(p), b).unwrap();
        }
        let scanned = scan(&dir);
        let m = scanned[0].manifest.as_ref().unwrap();
        let names: Vec<&str> = m.asset.iter().map(|a| a.name.as_str()).collect();
        // sorted by relative path
        assert_eq!(
            names,
            vec!["loose.dat", "_t_a.x", "_t_b.x", "_t_c.att", "x.dds"]
        );
        let by = |n: &str| m.asset.iter().find(|a| a.name == n).unwrap();
        // subfolder path relative to the folder IS the row dir
        assert_eq!(by("_t_a.x").dir, "train\\mytrain");
        assert_eq!(by("_t_b.x").dir, "train\\mytrain\\lod");
        assert_eq!(by("_t_c.att").dir, "train\\other");
        // a root file takes the folder's own path as its dir
        assert_eq!(by("x.dds").dir, "assets");
        assert_eq!(by("loose.dat").dir, "assets");
        // the payload path is mod-relative with '/'
        assert_eq!(by("_t_a.x").file, "assets/train/mytrain/_t_a.x");
        assert_eq!(by("loose.dat").file, "assets/loose.dat");
        // host/override carry over; every op is single-file form now
        assert!(m.asset.iter().all(|a| a.folder.is_none() && a.host == "vp"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn asset_folder_from_mod_root_uses_explicit_dir() {
        let dir = tmpdir("assetroot");
        std::fs::create_dir_all(dir.join("m/sub")).unwrap();
        std::fs::write(
            dir.join("m/mod.toml"),
            "id=\"m\"\nname=\"M\"\nversion=\"1.0.0\"\nauthor=\"t\"\n\
             [[asset]]\nfolder=\".\"\ndir=\"train\\\\room\"\nhost=\"auto\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("m/_t_r.x"), b"r").unwrap();
        std::fs::write(dir.join("m/sub/_t_s.x"), b"s").unwrap();
        let scanned = scan(&dir);
        let m = scanned[0].manifest.as_ref().unwrap();
        let by = |n: &str| m.asset.iter().find(|a| a.name == n).unwrap();
        // root file: folder path is empty -> explicit dir
        assert_eq!(by("_t_r.x").dir, "train\\room");
        // subfolder file: subfolder path relative to the folder
        assert_eq!(by("_t_s.x").dir, "sub");
        assert_eq!(m.asset[0].host, crate::manifest::HOST_AUTO);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn asset_folder_duplicate_basenames_are_a_hard_error() {
        let dir = tmpdir("assetdup");
        std::fs::create_dir_all(dir.join("m/assets/a")).unwrap();
        std::fs::create_dir_all(dir.join("m/assets/b")).unwrap();
        std::fs::write(
            dir.join("m/mod.toml"),
            "id=\"m\"\nname=\"M\"\nversion=\"1.0.0\"\nauthor=\"t\"\n\
             [[asset]]\nfolder=\"assets\"\nhost=\"vp\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("m/assets/a/_t_dup.x"), b"a").unwrap();
        std::fs::write(dir.join("m/assets/b/_t_dup.x"), b"b").unwrap();
        let scanned = scan(&dir);
        let e = scanned[0].manifest.as_ref().unwrap_err().to_string();
        assert!(e.contains("two files named") && e.contains("globally unique"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn asset_folder_duplicate_across_sections_is_refused() {
        let dir = tmpdir("assetdup2");
        std::fs::create_dir_all(dir.join("m/assets")).unwrap();
        std::fs::write(
            dir.join("m/mod.toml"),
            "id=\"m\"\nname=\"M\"\nversion=\"1.0.0\"\nauthor=\"t\"\n\
             [[asset]]\nfolder=\"assets\"\nhost=\"vp\"\n\
             [[asset]]\nname=\"_t_dup.x\"\ndir=\"train\\\\d\"\nhost=\"vp\"\nfile=\"f\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("m/assets/_t_dup.x"), b"a").unwrap();
        let scanned = scan(&dir);
        let e = scanned[0].manifest.as_ref().unwrap_err().to_string();
        assert!(e.contains("two [[asset]] sections for name"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_asset_folder_is_refused() {
        let dir = tmpdir("assetmissing");
        std::fs::create_dir_all(dir.join("m")).unwrap();
        std::fs::write(
            dir.join("m/mod.toml"),
            "id=\"m\"\nname=\"M\"\nversion=\"1.0.0\"\nauthor=\"t\"\n\
             [[asset]]\nfolder=\"nope\"\nhost=\"vp\"\n",
        )
        .unwrap();
        let scanned = scan(&dir);
        let e = scanned[0].manifest.as_ref().unwrap_err().to_string();
        assert!(e.contains("is not a directory"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_manifests_are_skipped_loudly_not_fatal() {
        let dir = tmpdir("mixed");
        std::fs::create_dir_all(dir.join("good")).unwrap();
        std::fs::create_dir_all(dir.join("bad")).unwrap();
        std::fs::create_dir_all(dir.join("future")).unwrap();
        std::fs::create_dir_all(dir.join("nomanifest")).unwrap();
        std::fs::write(
            dir.join("good/mod.toml"),
            "id=\"good\"\nname=\"G\"\nversion=\"1.0.0\"\nauthor=\"t\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("bad/mod.toml"), "id = 5\n").unwrap();
        std::fs::write(
            dir.join("future/mod.toml"),
            "id=\"f\"\nname=\"F\"\nversion=\"1.0.0\"\nauthor=\"t\"\n[native]\nabi=1\n",
        )
        .unwrap();
        let scanned = scan(&dir);
        // all four dirs report; a dir without mod.toml is a LOUD error entry
        assert_eq!(scanned.len(), 4);
        let nomanifest = scanned.iter().find(|s| s.dir.ends_with("nomanifest")).unwrap();
        assert!(nomanifest.manifest.is_err());
        let good = scanned.iter().find(|s| s.dir.ends_with("good")).unwrap();
        assert!(good.manifest.is_ok());
        let bad = scanned.iter().find(|s| s.dir.ends_with("bad")).unwrap();
        assert!(bad.manifest.is_err());
        let future = scanned.iter().find(|s| s.dir.ends_with("future")).unwrap();
        let err = future.manifest.as_ref().unwrap_err().to_string();
        assert!(err.contains("lands with M4+"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
