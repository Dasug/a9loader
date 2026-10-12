//! Mods-dir scanning — every `<mods_dir>/<mod>/mod.toml`, parsed and
//! validated, sorted deterministically by directory name. Parse/validation
//! failures produce a per-mod error entry (the mod is skipped loudly; other
//! mods still load).
//!
//! Manifests carrying sections designed for M3/M4 (`[[pack]]`, `[[csv]]`,
//! `[[file]]`, `[native]`) are refused with a note pointing at the milestone
//! that implements them — refusing beats half-executing a design the current
//! loader cannot honor.

use crate::manifest::Manifest;
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
        .collect();
    dirs.sort();
    let mut out = Vec::new();
    for dir in dirs {
        let manifest_path = dir.join("mod.toml");
        let manifest = std::fs::read_to_string(&manifest_path)
            .map_err(|e| ModError::Io(format!("mod.toml: {e}")))
            .and_then(|text| parse_manifest(&text))
            .and_then(|m| {
                m.validate()?;
                Ok(m)
            });
        out.push(ScannedMod { dir, manifest });
    }
    out
}

fn parse_manifest(text: &str) -> Result<Manifest, ModError> {
    match toml::from_str::<Manifest>(text) {
        Ok(m) => Ok(m),
        Err(e) => {
            let msg = e.to_string();
            // friendly pointer for designed-but-not-yet-supported sections
            for (name, milestone) in [
                ("pack", "M3 (runtime overlay)"),
                ("csv", "M3 (runtime overlay)"),
                ("file", "M3 (boot-time file copies)"),
                ("native", "M4+ (mods that ship code)"),
            ] {
                if msg.contains(&format!("unknown field `{name}`")) {
                    return Err(ModError::Parse(format!(
                        "section [[{name}]] is designed but lands with {milestone}; this loader build does not support it yet"
                    )));
                }
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
            "id=\"f\"\nname=\"F\"\nversion=\"1.0.0\"\nauthor=\"t\"\n[[pack]]\npack=\"mtentr0\"\n",
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
        assert!(err.contains("lands with M3"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
