//! `mod.toml` — the mod manifest schema (PLAN.md §4 v3). Sections are by
//! mechanism; every section is executed by the loader at boot. The schema is
//! strict: unknown fields refuse the whole mod (typos must be loud, not
//! silently ignored — the FORM_DUMMY lesson), and `validate()` enforces
//! per-handler parameter shapes.
//!
//! Sections designed for M3/M4 (`[[pack]]`, `[[csv]]`, `[[file]]`, `[native]`)
//! are NOT schema fields yet — a manifest carrying them is refused with a
//! pointer to the milestone that implements them (see [`crate::scan`]).
//!
//! The M3/M5/M6 section structs live in [`sections`]; the per-section checks
//! behind [`Manifest::validate`] live in [`validate`].

use crate::ModError;
use serde::{Deserialize, Serialize};

mod sections;
mod validate;

pub use sections::{
    AssetSection, CsvMode, CsvSection, FileSection, LooseAssetSection, PackEntry, PackSection,
    HOST_AUTO,
};

pub const DEFAULT_PRIORITY: i64 = 50;

/// Serialize is derived for the loader's legacy-ini → manifest auto-conversion
/// (it writes the converted `mod.toml`).
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: semver::Version,
    pub author: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    /// Exact build ids this mod's memory effects support ("5.00.5634").
    /// Pure data mods may omit it; the gate applies to [[patch]]/[[hook]].
    #[serde(default)]
    pub game_builds: Vec<String>,
    #[serde(default = "default_priority")]
    pub priority: i64,
    #[serde(default)]
    pub supplies: Vec<String>,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub conflicts: Vec<String>,
    #[serde(default)]
    pub patch: Vec<Patch>,
    #[serde(default)]
    pub hook: Vec<Hook>,
    /// family-B pack entry overlays (me*/mt* packs) — merged in memory at the
    /// game's open of `data/<pack>.bin` and served via the loader-managed
    /// cache (M3)
    #[serde(default)]
    pub pack: Vec<PackSection>,
    /// csvp table overlays — whole-table replace (default) or per-row patches
    /// (`mode = "rows"`): served at the player's csvp load (M3/M5)
    #[serde(default)]
    pub csv: Vec<CsvSection>,
    /// genuinely-new i.dat rows whose payloads are appended to a family-A host
    /// pack's decoded stream, merged in-memory at the game's load (M5 W1)
    #[serde(default)]
    pub asset: Vec<AssetSection>,
    /// genuinely-new files copied into the game tree at boot, before the game
    /// enumerates, journaled in `<mods_dir>/journal.json` (M3). Never
    /// overwrites an existing file — that is [[pack]]/[[csv]] territory.
    #[serde(default)]
    pub file: Vec<FileSection>,
    /// game files the game opens BY PATH (not content inside a pack) — the
    /// loader serves the mod's bytes in place of the file on disk, matched by
    /// a case-insensitive glob over the game's own open path (M6)
    #[serde(default)]
    pub loose_asset: Vec<LooseAssetSection>,
}

fn default_priority() -> i64 {
    DEFAULT_PRIORITY
}

/// A raw data write, gated on expected original bytes.
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Patch {
    /// anchor name from the build address book, or `{ rva = 0x…, expect = "…" }`
    /// (raw targets REQUIRE their own expect — the diagnostic escape hatch)
    pub target: TargetRef,
    /// byte offset from the anchor (e.g. whitelist entry index * 4)
    #[serde(default)]
    pub offset: u32,
    /// replacement bytes, hex
    pub data: String,
    /// expected original bytes at rva+offset, hex — verified before writing
    /// (required for raw targets; optional but recommended for anchors)
    #[serde(default)]
    pub expect: Option<String>,
}

/// A function replacement by a named built-in handler.
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    pub target: TargetRef,
    /// "train_list" | "nop"
    pub handler: String,
    /// train_list: `allow` (keep only the listed forms, default), `add`
    /// (offer the listed forms in addition to the shipped whitelist) or
    /// `deny` (drop the listed forms)
    #[serde(default)]
    pub mode: Option<ListMode>,
    /// train_list: allow/deny list file, relative to the mod dir
    #[serde(default)]
    pub list: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ListMode {
    /// keep only the listed forms (a mod's list declares the offered set;
    /// several `allow` mods compose as the union of their lists)
    Allow,
    /// offer the listed forms *in addition to* the shipped purchase whitelist
    /// (and any `allow` list) — what a new-train mod wants
    Add,
    /// drop the listed forms (subtracted last: a deny always wins)
    Deny,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
#[serde(untagged)]
pub enum TargetRef {
    /// Named anchor — resolved per running build via the address book.
    Anchor(String),
    /// Raw RVA + mandatory expect bytes — diagnostic escape hatch only.
    Raw { rva: u32, expect: String },
}

impl TargetRef {
    /// The plan-overlap key: two effects with the same key write the same
    /// bytes and are a hard conflict.
    pub fn key(&self, offset: u32) -> String {
        match self {
            TargetRef::Anchor(a) => a.clone(),
            TargetRef::Raw { rva, .. } => format!("raw:{rva:#x}+{offset}"),
        }
    }
    pub fn anchor(&self) -> Option<&str> {
        match self {
            TargetRef::Anchor(a) => Some(a),
            TargetRef::Raw { .. } => None,
        }
    }
}

impl Manifest {
    pub fn validate(&self) -> Result<(), ModError> {
        validate::validate(self)
    }
}

#[cfg(test)]
mod tests;
