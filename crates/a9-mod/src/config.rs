//! `a9loader.toml` — the loader config next to the game exe.
//!
//! Parsed leniently-but-loudly: a missing file means defaults; a broken file
//! means defaults plus warnings (the game must always boot); unknown keys are
//! **warnings**, not silent, because a typo like `mods_dr` must be visible in
//! the log, not quietly ignored.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
}

impl LogLevel {
    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "error" => Some(Self::Error),
            "warn" | "warning" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            _ => None,
        }
    }
}

/// How `[[pack]]` image effects serve their content (a9loader.toml
/// `pack_serving`, default "merge"):
/// - `merge`: the runtime overlay — hooked `CFile::Open` redirects the
///   pack open to a merged cache file (decode → edit → re-encode, budgeted).
/// - `inject`: the window-scoped injector — loader-owned memory spliced
///   into the family-B loader's worker-row pool at its runner call site
///. csvp `[[csv]]` tables keep the merge overlay in both
///   modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PackServing {
    Merge,
    Inject,
}

impl PackServing {
    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "merge" => Some(Self::Merge),
            "inject" => Some(Self::Inject),
            _ => None,
        }
    }
}

/// How `[[asset]]` new assets (family-A host packs + the `i.dat` index) serve
/// their content (a9loader.toml `asset_serving`, default "tailgate"):
/// - `tailgate`: the in-place append — the loader splices the game's one
///   family-A LZSS decode funnel (vp/tp/i.dat call sites, one same-image relay
///   cave) and appends the payloads / index rows straight into the shared
///   workspace's decoded image right after the decode. No re-encode, no cache
///   file, nothing on disk. (`pack_serving = "inject"` is orthogonal: it only
///   routes `[[pack]]` family-B effects.)
/// - `merge`: the cache overlay — decode → append → re-encode the host
///   pack to `<mods_dir>/cache/` and redirect its `CFile::Open`. The automatic
///   FALLBACK whenever the tailgate cannot be used (missing anchor/expect
///   bytes, a failed splice, an unreadable declared size, or a host whose
///   payloads do not fit the workspace slack).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AssetServing {
    Tailgate,
    Merge,
}

impl AssetServing {
    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "tailgate" => Some(Self::Tailgate),
            "merge" => Some(Self::Merge),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub log_level: LogLevel,
    /// mods folder, relative to the game exe dir
    pub mods_dir: PathBuf,
    pub allow_conflicts: bool,
    pub disabled: Vec<String>,
    pub sequence: Option<Vec<String>>,
    /// false = the loader-off escape hatch: no scan, no effects, no hooks —
    /// pure proxy passthrough, fully vanilla boot
    pub enabled: bool,
    /// per-mod priority overrides (mod id -> priority)
    pub priorities: BTreeMap<String, i64>,
    /// human-visible complaints (unknown keys, bad values)
    pub warnings: Vec<String>,
    /// A diagnostic hook — **OFF BY DEFAULT, not part of normal operation**: see
    /// `probe.rs`. When true the loader hooks the four runtime fetch anchors
    /// trampoline-only and logs one line per fetch call (obj / composed key).
    /// Diagnostics only — no serving behavior changes; fail-open per anchor.
    pub probe_fetch: bool,
    /// A diagnostic hook — **OFF BY DEFAULT, not part of normal operation**: see
    /// `probe_window.rs`. When true the loader hooks the big purchase/list
    /// window (`train_list_big_list_window`) trampoline-only and dumps the
    /// spec-table manager's three index vectors (+0x30 unfiltered / +0x48 the
    /// list the UI reads / +0x60 the type-group list) on the way in and on the
    /// way out of every call that CHANGES a size — the observable of the
    /// type-cycle quirk. Diagnostics only; no behaviour change.
    pub probe_window: bool,
    /// Build the injected trains' car data by driving the game's own car-model
    /// loader (see the loader's `build_cars`). Off by default: it calls into game
    /// code with a loader-built pattern row, so it is opt-in until proven.
    pub car_build: bool,
    /// DIAGNOSTIC ONLY: hook `train_row_car_wiring` and log the train object's
    /// per-car records (the sub-index it feeds to `aModelIdx[sub - 1]`). Off by
    /// default; not part of normal loader operation.
    pub probe_consist: bool,
    /// DIAGNOSTIC ONLY — **OFF BY DEFAULT**: one line per file open the loader
    /// intercepts (`open trace: <path>`). This used to follow `log_level = debug`,
    /// which made a debug boot 150+ lines of noise; `a9ml classify` answers the same
    /// "where is this file consumed" question offline.
    pub probe_opens: bool,
    /// DIAGNOSTIC ONLY — **OFF BY DEFAULT**: at every `train_list_builder` call,
    /// dump named spec rows (the model array, the per-car block and the scalar
    /// fields the panel/selector read) plus the 18 name lanes of the injected
    /// rows. This is the instrument that solved the wrong-mesh and selector
    /// questions, so it is kept — but it is the most verbose diagnostic in the
    /// loader and no longer belongs in a normal boot.
    pub probe_rows: bool,
    /// A diagnostic hook — **OFF BY DEFAULT, not part of normal operation**: see
    /// `probe_save.rs`. When true the loader hooks the four save-side mmf
    /// helpers (`mmf_paged_write` / `mmf_paged_write_bulk` /
    /// `save_mmf_write_block` / `mmf_read_u32_vector`) trampoline-only and logs
    /// the caller RVA + size + first bytes of every save-path write (capped),
    /// plus a per-distinct-caller map and a size histogram — the observable
    /// that says whether a formation's car block is one bulk `0xf8` write or
    /// ten `0x18` writes, and which caller RVA is the group-record writer.
    /// Diagnostics only; no behaviour change.
    pub probe_save: bool,
    /// how [[pack]] image effects serve (see [`PackServing`]); "inject" (the
    /// default) = the window-scoped famb injector (loader memory, no cache
    /// file), "merge" = the cache overlay
    pub pack_serving: PackServing,
    /// how [[asset]] new assets serve (see [`AssetServing`]); "tailgate" (the
    /// default) = the in-place decode append, "merge" = the cache
    /// overlay
    pub asset_serving: AssetServing,
    /// The family-A decode workspace swap's OUT size in MiB. `0` (default) =
    /// AUTO — replace the game's 200 MB buffer only when the planned family-A
    /// content (vanilla declared size + appended payloads) does not fit it;
    /// a value forces that size (the "as much as the machine can take" soft
    /// limit). The new buffer comes from the game's own `_aligned_malloc` so
    /// its teardown frees it cleanly, and the out CAP is raised with it.
    pub workspace_out_mb: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            log_level: LogLevel::Info,
            mods_dir: PathBuf::from("mods"),
            allow_conflicts: false,
            disabled: Vec::new(),
            sequence: None,
            enabled: true,
            priorities: BTreeMap::new(),
            warnings: Vec::new(),
            probe_fetch: false,
            probe_window: false,
            car_build: false,
            probe_consist: false,
            probe_rows: false,
            probe_opens: false,
            probe_save: false,
            pack_serving: PackServing::Inject,
            asset_serving: AssetServing::Tailgate,
            workspace_out_mb: 0,
        }
    }
}

impl Config {
    /// Resolution inputs (for [`crate::resolve`]).
    pub fn resolve_config(&self) -> crate::resolve::ResolveConfig {
        crate::resolve::ResolveConfig {
            sequence: self.sequence.clone(),
            priorities: self.priorities.clone(),
            disabled: self.disabled.clone(),
            allow_conflicts: self.allow_conflicts,
        }
    }
}

/// Read and parse `a9loader.toml` at `path`. Missing file → defaults.
/// Broken/unknown bits → defaults + warnings (never a failed boot).
pub fn read(path: &Path) -> Config {
    let mut cfg = Config::default();
    let Ok(text) = std::fs::read_to_string(path) else {
        return cfg; // no config file: defaults, silent (that's normal)
    };
    let table: toml::Table = match toml::from_str(&text) {
        Ok(t) => t,
        Err(e) => {
            cfg.warnings
                .push(format!("a9loader.toml unreadable ({e}); using defaults"));
            return cfg;
        }
    };
    for (key, val) in &table {
        match key.as_str() {
            "log_level" => match val.as_str().and_then(LogLevel::parse) {
                Some(l) => cfg.log_level = l,
                None => cfg.warnings.push(format!(
                    "log_level must be error|warn|info|debug, got {val:?}"
                )),
            },
            "mods_dir" => match val.as_str() {
                Some(s) if !s.trim().is_empty() && !s.contains("..") => {
                    cfg.mods_dir = PathBuf::from(s.trim_end_matches(['/', '\\']));
                }
                _ => cfg
                    .warnings
                    .push(format!("mods_dir must be a simple relative dir name, got {val:?}")),
            },
            "allow_conflicts" => match val.as_bool() {
                Some(b) => cfg.allow_conflicts = b,
                None => cfg.warnings.push(format!("allow_conflicts must be a bool, got {val:?}")),
            },
            "enabled" => match val.as_bool() {
                Some(b) => cfg.enabled = b,
                None => cfg.warnings.push(format!("enabled must be a bool, got {val:?}")),
            },
            "probe_fetch" => match val.as_bool() {
                Some(b) => cfg.probe_fetch = b,
                None => cfg
                    .warnings
                    .push(format!("probe_fetch must be a bool, got {val:?}")),
            },
            "probe_window" => match val.as_bool() {
                Some(b) => cfg.probe_window = b,
                None => cfg
                    .warnings
                    .push(format!("probe_window must be a bool, got {val:?}")),
            },
            "probe_consist" => match val.as_bool() {
                Some(b) => cfg.probe_consist = b,
                None => cfg
                    .warnings
                    .push(format!("probe_consist must be a bool, got {val:?}")),
            },
            "probe_opens" => match val.as_bool() {
                Some(b) => cfg.probe_opens = b,
                None => cfg
                    .warnings
                    .push(format!("probe_opens must be a bool, got {val:?}")),
            },
            "probe_rows" => match val.as_bool() {
                Some(b) => cfg.probe_rows = b,
                None => cfg
                    .warnings
                    .push(format!("probe_rows must be a bool, got {val:?}")),
            },
            "car_build" => match val.as_bool() {
                Some(b) => cfg.car_build = b,
                None => cfg
                    .warnings
                    .push(format!("car_build must be a bool, got {val:?}")),
            },
            "probe_save" => match val.as_bool() {
                Some(b) => cfg.probe_save = b,
                None => cfg
                    .warnings
                    .push(format!("probe_save must be a bool, got {val:?}")),
            },
            "pack_serving" => match val.as_str().and_then(PackServing::parse) {
                Some(m) => cfg.pack_serving = m,
                None => cfg.warnings.push(format!(
                    "pack_serving must be merge|inject, got {val:?}"
                )),
            },
            "asset_serving" => match val.as_str().and_then(AssetServing::parse) {
                Some(m) => cfg.asset_serving = m,
                None => cfg.warnings.push(format!(
                    "asset_serving must be tailgate|merge, got {val:?}"
                )),
            },
            "workspace_out_mb" => match val.as_integer() {
                Some(n) if (0..=3584).contains(&n) => cfg.workspace_out_mb = n as u32,
                _ => cfg.warnings.push(format!(
                    "workspace_out_mb must be an integer 0..=3584 (MiB; 0 = auto), got {val:?}"
                )),
            },
            "disabled" => match val.as_array() {
                Some(a) if a.iter().all(|v| v.is_str()) => {
                    cfg.disabled = a
                        .iter()
                        .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                        .filter(|s| !s.is_empty())
                        .collect();
                }
                _ => cfg.warnings.push(format!("disabled must be an array of mod ids, got {val:?}")),
            },
            "order" => match val.as_table() {
                Some(t) => {
                    for (k, v) in t {
                        if k == "sequence" {
                            match v.as_array() {
                                Some(a) if a.iter().all(|x| x.is_str()) => {
                                    cfg.sequence = Some(
                                        a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
                                    );
                                }
                                _ => cfg
                                    .warnings
                                    .push("order.sequence must be an array of mod ids".into()),
                            }
                        } else {
                            match v.as_integer() {
                                Some(p) => {
                                    cfg.priorities.insert(k.clone(), p);
                                }
                                None => cfg.warnings.push(format!(
                                    "order.{k} must be an integer priority override, got {v:?}"
                                )),
                            }
                        }
                    }
                }
                None => cfg.warnings.push("order must be a table ([order])".into()),
            },
            other => cfg
                .warnings
                .push(format!("unknown a9loader.toml key {other:?} (ignored)")),
        }
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_missing() {
        let cfg = read(Path::new("/nonexistent/a9loader.toml"));
        assert_eq!(cfg, Config::default());
        assert!(cfg.warnings.is_empty());
    }

    #[test]
    fn parses_full_config() {
        let text = r#"
log_level = "debug"
mods_dir = "a9mods"
allow_conflicts = true
disabled = ["broken"]
probe_fetch = false

[order]
sequence = ["b", "a"]
jp-liveries = 10
"#;
        let dir = std::env::temp_dir().join("a9ml-cfg-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a9loader.toml");
        std::fs::write(&path, text).unwrap();
        let cfg = read(&path);
        assert_eq!(cfg.log_level, LogLevel::Debug);
        assert_eq!(cfg.mods_dir, PathBuf::from("a9mods"));
        assert!(cfg.allow_conflicts);
        assert_eq!(cfg.disabled, vec!["broken"]);
        assert_eq!(cfg.sequence.as_deref(), Some(&["b".to_string(), "a".to_string()][..]));
        assert_eq!(cfg.priorities.get("jp-liveries"), Some(&10));
        assert!(!cfg.probe_fetch);
        assert!(cfg.warnings.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_probe_save_flag() {
        let dir = std::env::temp_dir().join("a9ml-cfg-test-probe-save");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a9loader.toml");
        std::fs::write(&path, "probe_save = true\n").unwrap();
        let cfg = read(&path);
        assert!(cfg.probe_save);
        assert!(cfg.warnings.is_empty());
        std::fs::write(&path, "probe_save = \"yes\"\n").unwrap();
        let cfg = read(&path);
        assert!(!cfg.probe_save);
        assert!(cfg.warnings.iter().any(|w| w.contains("probe_save")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_probe_fetch_flag() {
        let dir = std::env::temp_dir().join("a9ml-cfg-test-probe");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a9loader.toml");
        std::fs::write(&path, "probe_fetch = true\n").unwrap();
        let cfg = read(&path);
        assert!(cfg.probe_fetch);
        assert!(cfg.warnings.is_empty());
        std::fs::write(&path, "probe_fetch = \"yes\"\n").unwrap();
        let cfg = read(&path);
        assert!(!cfg.probe_fetch);
        assert!(cfg.warnings.iter().any(|w| w.contains("probe_fetch")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_probe_window_flag() {
        let dir = std::env::temp_dir().join("a9ml-cfg-test-probe-window");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a9loader.toml");
        std::fs::write(&path, "probe_window = true\n").unwrap();
        let cfg = read(&path);
        assert!(cfg.probe_window);
        assert!(cfg.warnings.is_empty());
        std::fs::write(&path, "probe_window = \"yes\"\n").unwrap();
        let cfg = read(&path);
        assert!(!cfg.probe_window);
        assert!(cfg.warnings.iter().any(|w| w.contains("probe_window")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_pack_serving() {
        let dir = std::env::temp_dir().join("a9ml-cfg-test-serving");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a9loader.toml");
        std::fs::write(&path, "pack_serving = \"inject\"\n").unwrap();
        let cfg = read(&path);
        assert_eq!(cfg.pack_serving, PackServing::Inject);
        assert!(cfg.warnings.is_empty());
        std::fs::write(&path, "pack_serving = \"MERGE\"\n").unwrap();
        let cfg = read(&path);
        assert_eq!(cfg.pack_serving, PackServing::Merge);
        std::fs::write(&path, "pack_serving = \"yes\"\n").unwrap();
        let cfg = read(&path);
        assert_eq!(cfg.pack_serving, PackServing::Inject); // default on bad value
        assert!(cfg.warnings.iter().any(|w| w.contains("pack_serving")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_asset_serving() {
        let dir = std::env::temp_dir().join("a9ml-cfg-test-asset-serving");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a9loader.toml");
        // default is tailgate
        let cfg = Config::default();
        assert_eq!(cfg.asset_serving, AssetServing::Tailgate);
        std::fs::write(&path, "asset_serving = \"merge\"\n").unwrap();
        let cfg = read(&path);
        assert_eq!(cfg.asset_serving, AssetServing::Merge);
        assert!(cfg.warnings.is_empty());
        std::fs::write(&path, "asset_serving = \"TAILGATE\"\n").unwrap();
        let cfg = read(&path);
        assert_eq!(cfg.asset_serving, AssetServing::Tailgate);
        std::fs::write(&path, "asset_serving = \"yes\"\n").unwrap();
        let cfg = read(&path);
        assert_eq!(cfg.asset_serving, AssetServing::Tailgate); // default on bad value
        assert!(cfg.warnings.iter().any(|w| w.contains("asset_serving")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_keys_and_bad_values_warn_loudly() {
        let text = "mods_dr = \"mods\"\nlog_level = \"loud\"\n[order]\nseq = \"x\"\n";
        let dir = std::env::temp_dir().join("a9ml-cfg-test2");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a9loader.toml");
        std::fs::write(&path, text).unwrap();
        let cfg = read(&path);
        assert_eq!(cfg, {
            let mut d = Config::default();
            d.warnings = cfg.warnings.clone();
            d
        }); // all values defaulted
        assert!(cfg.warnings.iter().any(|w| w.contains("mods_dr")));
        assert!(cfg.warnings.iter().any(|w| w.contains("log_level")));
        assert!(cfg.warnings.iter().any(|w| w.contains("order.seq")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_file_defaults_with_warning() {
        let dir = std::env::temp_dir().join("a9ml-cfg-test3");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a9loader.toml");
        std::fs::write(&path, "not [ toml").unwrap();
        let cfg = read(&path);
        assert_eq!(cfg.mods_dir, PathBuf::from("mods"));
        assert!(cfg.warnings.iter().any(|w| w.contains("unreadable")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
