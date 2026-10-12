//! `a9loader.toml` — the loader config next to the game exe (PLAN.md §4.7).
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

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub log_level: LogLevel,
    /// mods folder, relative to the game exe dir
    pub mods_dir: PathBuf,
    pub allow_conflicts: bool,
    pub disabled: Vec<String>,
    pub sequence: Option<Vec<String>>,
    /// per-mod priority overrides (mod id -> priority)
    pub priorities: BTreeMap<String, i64>,
    /// human-visible complaints (unknown keys, bad values)
    pub warnings: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            log_level: LogLevel::Info,
            mods_dir: PathBuf::from("mods"),
            allow_conflicts: false,
            disabled: Vec::new(),
            sequence: None,
            priorities: BTreeMap::new(),
            warnings: Vec::new(),
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

    pub fn log_enabled(&self, level: LogLevel) -> bool {
        level <= self.log_level
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
        assert!(cfg.warnings.is_empty());
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
