//! `mod.toml` — the mod manifest schema (PLAN.md §4 v3). Sections are by
//! mechanism; every section is executed by the loader at boot. The schema is
//! strict: unknown fields refuse the whole mod (typos must be loud, not
//! silently ignored — the FORM_DUMMY lesson), and `validate()` enforces
//! per-handler parameter shapes.
//!
//! Sections designed for later milestones (`[native]`) are NOT schema fields
//! yet — a manifest carrying them is refused with a pointer to the milestone
//! that implements them (see [`Manifest::parse`]).
//!
//! The M3/M5/M6 section structs live in [`sections`]; the per-section checks
//! behind [`Manifest::validate`] live in [`validate`].

use crate::ModError;
use serde::{Deserialize, Serialize};

mod sections;
mod validate;

pub use sections::{
    AssetSection, CsvMode, CsvSection, LooseAssetSection, PackEntry, PackSection, TrainModel,
    TrainSection, HOST_AUTO,
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
    /// game files the game opens BY PATH (not content inside a pack) — the
    /// loader serves the mod's bytes in place of the file on disk, matched by
    /// a case-insensitive glob over the game's own open path (M6)
    #[serde(default)]
    pub loose_asset: Vec<LooseAssetSection>,
    /// brand-new train types beyond the 417-name FORM catalog (M7-2) — a
    /// compiler over the row-merge/asset/row-injection primitives: the author
    /// declares the train, the loader allocates its id + FORM name, derives the
    /// four csvp rows, the icons and the visibility entry
    #[serde(default)]
    pub train: Vec<TrainSection>,
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

    /// Parse a `mod.toml`. The schema is strict (`deny_unknown_fields`); a
    /// section the loader has retired or not yet implemented would otherwise
    /// surface as a bare "unknown field" — [`retired_section_error`] maps it to
    /// a pointer at the replacement.
    pub fn parse(text: &str) -> Result<Self, ModError> {
        match toml::from_str::<Self>(text) {
            Ok(m) => Ok(m),
            Err(e) => {
                let msg = e.to_string();
                Err(ModError::Parse(match retired_section_error(text) {
                    Some(friendly) => friendly,
                    None => msg,
                }))
            }
        }
    }
}

/// The friendlier refusal for a manifest that still declares a section the
/// loader has retired (`[[file]]`) or not yet implemented (`[native]`).
/// Returns `None` when the manifest declares neither.
///
/// Detected from the manifest TEXT, never from the toml error message: several
/// supported sections legitimately carry a field called `file`
/// (`[[pack.entry]]`, `[[csv]]`, `[[asset]]`, `[[loose_asset]]`), so matching on
/// "unknown field `file`" turned any unrelated parse error in such a mod into a
/// false "[[file]] is retired" report — which is exactly what happened to four
/// of the live mods.
fn retired_section_error(text: &str) -> Option<String> {
    let declares = |header: &str| text.lines().any(|l| l.trim().starts_with(header));
    // Top-level keys can only appear before the first section header — scanning the
    // whole text for a `file =` line would flag the perfectly valid `file` field of
    // `[[pack.entry]]`, `[[csv]]`, `[[asset]]` and `[[loose_asset]]`.
    let head: String = text
        .lines()
        .take_while(|l| !l.trim_start().starts_with('['))
        .collect::<Vec<_>>()
        .join("\n");
    let retires_file = declares("[[file]]")
        || head.lines().any(|l| {
            let t = l.trim_start();
            t.starts_with("file ") || t.starts_with("file=") || t.starts_with("file\t")
        });
    if retires_file {
        return Some(
            "section [[file]] is retired: the loader no longer writes into the game folder. \
             Serve pack content with [[asset]] and game files opened by path with \
             [[loose_asset]] instead. If the manifest only lists it as an empty key \
             (`file = []`), just delete that line"
                .into(),
        );
    }
    if declares("[native]") {
        return Some(
            "section [native] is designed but lands with M4+ (mods that ship code); \
             this loader build does not support it yet"
                .into(),
        );
    }
    None
}

#[cfg(test)]
mod tests;
