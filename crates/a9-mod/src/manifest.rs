//! `mod.toml` — the mod manifest schema (PLAN.md §4 v3). Sections are by
//! mechanism; every section is executed by the loader at boot. The schema is
//! strict: unknown fields refuse the whole mod (typos must be loud, not
//! silently ignored — the FORM_DUMMY lesson), and `validate()` enforces
//! per-handler parameter shapes.
//!
//! Sections designed for M3/M4 (`[[pack]]`, `[[csv]]`, `[[file]]`, `[native]`)
//! are NOT schema fields yet — a manifest carrying them is refused with a
//! pointer to the milestone that implements them (see [`crate::scan`]).

use crate::hex;
use crate::ModError;
use serde::{Deserialize, Serialize};

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

// -------------------------------------------------------------- M3 sections

/// `[[pack]]` — entry operations on one family-B pack (`data/<pack>.bin`).
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct PackSection {
    /// pack name without path/extension, e.g. "mtentr0" (matches the game's
    /// open of `data\mtentr0.bin`); csvp tables belong in [[csv]]
    pub pack: String,
    #[serde(default)]
    pub entry: Vec<PackEntry>,
}

/// `[[pack.entry]]` — replace (name exists) or add (name is new).
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct PackEntry {
    /// entry name as stored in the pack (e.g. "_d_h510_keifu_8000_tr_01.dds")
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// payload file, relative to the mod dir
    pub file: String,
}

/// `[[csv]]` — an operation on one table inside csvp by name (e.g.
/// "train_form.csv").
///
/// `mode` decides how `file` composes with the shipped table:
/// - `"replace"` (the default, and the only mode the pre-W2 loader had):
///   the file IS the whole table — it takes the table over. Existing
///   whole-table mods (english-train-names) keep this exact behavior with no
///   manifest change, so the default stays `replace` rather than switching to
///   `rows`: a whole table re-parsed as row patches would choke on its
///   header/comment rows (empty first column = no row identity).
/// - `"rows"`: the file holds just the rows to edit/add, keyed by the first
///   column. Independent mods can then patch the same table without a
///   whole-table conflict, and a mod can append a new row.
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct CsvSection {
    pub name: String,
    /// CSV payload file, relative to the mod dir (whole table or patch rows)
    pub file: String,
    #[serde(default)]
    pub mode: CsvMode,
    /// The patch rows' first-column keys, filled by [`crate::scan`] from the
    /// file for `mode = "rows"` — derived state, never part of the manifest
    /// (skipped by serde; the resolver uses it for per-row overlap keys).
    #[serde(skip)]
    pub row_keys: Vec<String>,
}

/// How a `[[csv]]` section's file composes with the shipped table.
#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum CsvMode {
    /// the file IS the whole table (default — backward compatible)
    #[default]
    Replace,
    /// the file is a set of row patches (row identity = first column)
    Rows,
}

/// `host = "auto"`: resolve the host pack at load time from the embedded
/// per-build `dir -> pack` index ([`crate::dirindex`]). It is the default in
/// both asset forms; an explicit family-A pack key always overrides it.
pub const HOST_AUTO: &str = "auto";

fn default_host() -> String {
    HOST_AUTO.to_string()
}

/// `[[asset]]` — a genuinely-new i.dat row whose payload is appended to a
/// family-A host pack's decoded stream (M5 W1). Served only through the merge
/// overlay (inject has no family-A seam).
///
/// Exactly one of two forms:
/// - **single-file**: `name` + `file` (one row per section);
/// - **folder**: `folder = "<dir inside the mod>"` — every file under it,
///   recursively, becomes one row. [`crate::scan`] expands folder sections
///   into single-file-form ops before resolution, so nothing downstream sees
///   the folder form (`folder` is `None` on an expanded op).
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct AssetSection {
    /// bare i.dat / master-map key (no directory), e.g. "_t_mynew_a.x" — the
    /// basename the train CSVs' `train\<dir>\<model>.x` composition produces.
    /// Globally unique per build. Single-file form only (empty in folder form).
    #[serde(default)]
    pub name: String,
    /// i.dat `dir` field (consumer grouping, not part of the lookup key),
    /// e.g. "train\\mynew" (backslashes are allowed here). Optional: a folder
    /// form derives each row's dir from the file's subfolder (see `folder`).
    #[serde(default)]
    pub dir: String,
    /// host pack key: `"auto"` (the default — resolved from the embedded
    /// dir index) or a family-A pack the loader can append to — "vp", "tp",
    /// "mcp", "ep" or "cp" (must classify `(Family::A, Opaque)`).
    #[serde(default = "default_host")]
    pub host: String,
    /// payload file inside the mod dir — shipped as the exact pack chunk, so
    /// the i.dat row's `len` is this file's size (a `.x` includes the 12-byte
    /// ADX0 header). Single-file form only.
    #[serde(default)]
    pub file: String,
    /// folder form: a folder inside the mod. Every file under it, recursively,
    /// becomes one row: the file's basename is the row `name` and its
    /// subfolder path (relative to the folder, `\`-separated) is the row
    /// `dir`. A file directly in the folder takes the folder's own path as its
    /// `dir`; when the folder IS the mod root (`.`), that falls back to the
    /// section's explicit `dir`. Mutually exclusive with `name`/`file`.
    #[serde(default)]
    pub folder: Option<String>,
    /// allow this asset to REPLACE a row that exists in the SHIPPED i.dat.
    /// Without it such a name collision is a hard refusal (the engine's
    /// master-map insert-or-find would otherwise silently overwrite a shipped
    /// asset). Concerns only shipped assets — a mod-vs-mod collision on the
    /// same name is resolved by load order (higher priority wins) and needs
    /// no flag. Applies to every file of a folder form.
    #[serde(default)]
    pub override_vanilla: bool,
}

/// `[[file]]` — a genuinely-new file copied into the game tree at boot.
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct FileSection {
    /// destination, relative to the game dir (e.g. "data/texture/x.dds")
    pub dest: String,
    /// source file, relative to the mod dir
    pub file: String,
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
        // id: lowercase slug
        let id = &self.id;
        if id.is_empty()
            || !id.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            || !id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'))
        {
            return Err(ModError::Invalid(format!(
                "id must be a lowercase slug [a-z0-9][a-z0-9._-]*, got {id:?}"
            )));
        }
        for (field, list) in [
            ("game_builds", &self.game_builds),
            ("supplies", &self.supplies),
            ("requires", &self.requires),
            ("conflicts", &self.conflicts),
        ] {
            if list.iter().any(|s| s.trim().is_empty()) {
                return Err(ModError::Invalid(format!(
                    "{field} contains an empty entry"
                )));
            }
        }
        if self.conflicts.iter().any(|c| c == &self.id) {
            return Err(ModError::Invalid(format!(
                "{} conflicts with itself",
                self.id
            )));
        }
        let mut keys = std::collections::BTreeSet::new();
        for (i, p) in self.patch.iter().enumerate() {
            let key = p.target.key(p.offset);
            if !keys.insert(format!("patch:{key}")) {
                return Err(ModError::Invalid(format!(
                    "two [[patch]] entries write the same target {key:?} (patch #{i})"
                )));
            }
            match hex::parse(&p.data) {
                Some(b) if !b.is_empty() => {}
                _ => {
                    return Err(ModError::Invalid(format!(
                        "patch {key}: `data` must be non-empty valid hex"
                    )));
                }
            }
            if let TargetRef::Raw { rva, expect } = &p.target {
                if hex::parse(expect).map_or(true, |b| b.is_empty()) {
                    return Err(ModError::Invalid(format!(
                        "patch {key}: raw rva {rva:#x} target requires non-empty `expect` hex"
                    )));
                }
            }
            if let Some(e) = &p.expect {
                if hex::parse(e).map_or(true, |b| b.is_empty()) {
                    return Err(ModError::Invalid(format!(
                        "patch {key}: `expect` must be non-empty valid hex"
                    )));
                }
            }
        }
        for (i, h) in self.hook.iter().enumerate() {
            let key = h.target.key(0);
            if !keys.insert(format!("hook:{key}")) {
                return Err(ModError::Invalid(format!(
                    "two effects write the same target {key:?} (hook #{i})"
                )));
            }
            match h.handler.as_str() {
                "train_list" => {
                    if h.list.as_deref().map_or(true, |s| s.trim().is_empty()) {
                        return Err(ModError::Invalid(format!(
                            "hook {key}: handler train_list requires `list` (a file in the mod dir)"
                        )));
                    }
                }
                "nop" => {
                    if h.mode.is_some() || h.list.is_some() {
                        return Err(ModError::Invalid(format!(
                            "hook {key}: handler nop takes no parameters"
                        )));
                    }
                }
                other => {
                    return Err(ModError::Invalid(format!(
                        "hook {key}: unknown handler {other:?} (known: train_list, nop)"
                    )));
                }
            }
            if let TargetRef::Raw { rva, expect } = &h.target {
                if hex::parse(expect).is_none() {
                    return Err(ModError::Invalid(format!(
                        "hook {key}: raw rva {rva:#x} `expect` is not valid hex"
                    )));
                }
            }
        }
        // [[pack]] — family-B entry overlays
        let mut pack_names = std::collections::BTreeSet::new();
        for p in &self.pack {
            if !p
                .pack
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                || p.pack.is_empty()
            {
                return Err(ModError::Invalid(format!(
                    "pack name must be a lowercase slug (e.g. \"mtentr0\"), got {:?}",
                    p.pack
                )));
            }
            if !pack_names.insert(p.pack.clone()) {
                return Err(ModError::Invalid(format!(
                    "two [[pack]] sections for pack {:?} — merge their entries into one",
                    p.pack
                )));
            }
            if p.pack == "csvp" {
                return Err(ModError::Invalid(
                    "csvp tables belong in [[csv]] sections, not [[pack]]".into(),
                ));
            }
            if p.entry.is_empty() {
                return Err(ModError::Invalid(format!(
                    "[[pack]] {} has no [[pack.entry]]",
                    p.pack
                )));
            }
            let mut names = std::collections::BTreeSet::new();
            for e in &p.entry {
                if e.name.trim().is_empty() || e.name.contains('\0') {
                    return Err(ModError::Invalid(format!(
                        "pack {} entry name must be a nonempty plain name, got {:?}",
                        p.pack, e.name
                    )));
                }
                if !names.insert(e.name.clone()) {
                    return Err(ModError::Invalid(format!(
                        "pack {} has two entries named {:?}",
                        p.pack, e.name
                    )));
                }
                if e.file.trim().is_empty() {
                    return Err(ModError::Invalid(format!(
                        "pack {} entry {:?} has no `file`",
                        p.pack, e.name
                    )));
                }
            }
        }
        // [[csv]] — csvp table overlays
        let mut csv_names = std::collections::BTreeSet::new();
        for c in &self.csv {
            if c.name.trim().is_empty() || c.name.contains('\0') {
                return Err(ModError::Invalid(format!(
                    "csv table name must be nonempty, got {:?}",
                    c.name
                )));
            }
            if !csv_names.insert(c.name.clone()) {
                return Err(ModError::Invalid(format!(
                    "two [[csv]] sections for table {:?}",
                    c.name
                )));
            }
            if c.file.trim().is_empty() {
                return Err(ModError::Invalid(format!(
                    "csv {:?} has no `file`",
                    c.name
                )));
            }
        }
        // [[file]] — boot copies of genuinely-new files
        let mut dests = std::collections::BTreeSet::new();
        for f in &self.file {
            let bad = |why: &str| {
                ModError::Invalid(format!("file dest {:?} {why}", f.dest))
            };
            if f.dest.trim().is_empty() {
                return Err(ModError::Invalid("[[file]] dest must not be empty".into()));
            }
            if f.dest.starts_with('/') || f.dest.starts_with('\\') || f.dest.contains(':') {
                return Err(bad("must be a relative path inside the game dir"));
            }
            if f.dest.split(['/', '\\']).any(|c| c == ".." || c.is_empty() && false) {
                return Err(bad("must not contain '..'"));
            }
            if !dests.insert(f.dest.clone()) {
                return Err(ModError::Invalid(format!(
                    "two [[file]] sections write dest {:?}",
                    f.dest
                )));
            }
            if f.file.trim().is_empty() {
                return Err(ModError::Invalid(format!(
                    "file dest {:?} has no source `file`",
                    f.dest
                )));
            }
        }
        // [[asset]] — a family-A append + an i.dat row (M5 W1), single-file
        // form (`name`+`file`) or folder form (`folder`, expanded at scan
        // time). Exactly one form per section.
        let mut asset_names = std::collections::BTreeSet::new();
        for a in &self.asset {
            let folder = a.folder.as_deref().map(str::trim).filter(|s| !s.is_empty());
            match folder {
                Some(folder) => {
                    if !a.name.trim().is_empty() || !a.file.trim().is_empty() {
                        return Err(ModError::Invalid(format!(
                            "asset folder {folder:?}: `name`/`file` are not allowed together \
                             with `folder` — a folder expands to one row per file"
                        )));
                    }
                    validate_mod_rel_path(folder, "asset folder")?;
                }
                None => {
                    if a.name.trim().is_empty() {
                        return Err(ModError::Invalid(
                            "asset name must be a nonempty plain name \
                             (or use `folder` to declare a whole folder)"
                                .into(),
                        ));
                    }
                    if a.file.trim().is_empty() {
                        return Err(ModError::Invalid(format!(
                            "asset {:?} has no `file`",
                            a.name
                        )));
                    }
                }
            }
            if a.name.contains('\0') || a.dir.contains('\0') {
                return Err(ModError::Invalid(
                    "asset name/dir must not contain NUL".into(),
                ));
            }
            // only the single-file form has a name yet; folder-derived names
            // are de-duplicated by the post-expansion validate()
            if !a.name.trim().is_empty() && !asset_names.insert(a.name.clone()) {
                return Err(ModError::Invalid(format!(
                    "two [[asset]] sections for name {:?} — a name is one row",
                    a.name
                )));
            }
            // host: "auto" (resolved at load time from the embedded dir index)
            // or a family-A opaque stream the loader can append to
            // (vp/tp/mcp/ep/cp) — never i.dat itself (that is the index)
            if !a.host.eq_ignore_ascii_case(HOST_AUTO) {
                let host_slug = !a.host.is_empty()
                    && a.host
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
                if !host_slug
                    || !matches!(
                        a9_formats::pack::classify(&a.host),
                        Some((a9_formats::pack::Family::A, a9_formats::pack::Container::Opaque))
                    )
                {
                    return Err(ModError::Invalid(format!(
                        "asset {:?}: host {:?} must be \"auto\" or a family-A pack the loader \
                         can append to (vp/tp/mcp/ep/cp)",
                        if a.name.is_empty() {
                            a.folder.as_deref().unwrap_or("")
                        } else {
                            &a.name
                        },
                        a.host
                    )));
                }
            }
        }
        Ok(())
    }
}

/// A mod-relative path (folder form): not absolute, no drive, no `..`.
fn validate_mod_rel_path(p: &str, what: &str) -> Result<(), ModError> {
    if p.starts_with('/') || p.starts_with('\\') || p.contains(':') {
        return Err(ModError::Invalid(format!(
            "{what} {p:?} must be a relative path inside the mod dir"
        )));
    }
    if p.split(['/', '\\']).any(|c| c == "..") {
        return Err(ModError::Invalid(format!(
            "{what} {p:?} must not contain '..'"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Manifest, toml::de::Error> {
        toml::from_str(text)
    }

    const GOOD: &str = r#"
id = "jp-liveries"
name = "JP 2022 liveries"
version = "1.0.0"
author = "dasug"
game_builds = ["5.00.5634"]
priority = 10
supplies = ["jp-livery-set"]
requires = ["a9-base-assets"]
conflicts = ["stock-liveries"]

[[hook]]
target = "train_list_filter"
handler = "train_list"
mode = "allow"
list = "liveries.txt"

[[patch]]
target = "train_list_whitelist"
offset = 24
data = "88 00 00 00"
expect = "bb 00 00 00"
"#;

    #[test]
    fn parses_and_validates_good_manifest() {
        let m = parse(GOOD).unwrap();
        m.validate().unwrap();
        assert_eq!(m.id, "jp-liveries");
        assert_eq!(m.priority, 10);
        assert_eq!(m.hook.len(), 1);
        assert_eq!(m.hook[0].mode, Some(ListMode::Allow));
        assert_eq!(m.patch[0].offset, 24);
        assert!(matches!(m.patch[0].target, TargetRef::Anchor(_)));
    }

    #[test]
    fn defaults_fill_in() {
        let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"0.1.0\"\nauthor=\"a\"\n").unwrap();
        m.validate().unwrap();
        assert_eq!(m.priority, DEFAULT_PRIORITY);
        assert!(m.game_builds.is_empty());
        assert!(m.patch.is_empty() && m.hook.is_empty());
    }

    #[test]
    fn train_list_modes_allow_add_deny() {
        // `add` (M5 W3): offer the listed forms on top of the shipped whitelist
        let m = parse(
            "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
             [[hook]]\ntarget=\"train_list_filter\"\nhandler=\"train_list\"\nmode=\"add\"\nlist=\"add.txt\"\n",
        )
        .unwrap();
        m.validate().unwrap();
        assert_eq!(m.hook[0].mode, Some(ListMode::Add));
        // an unknown mode is refused by the enum, like any typo
        assert!(parse(
            "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
             [[hook]]\ntarget=\"train_list_filter\"\nhandler=\"train_list\"\nmode=\"append\"\nlist=\"a.txt\"\n"
        )
        .is_err());
        // `add` still requires a list
        let m = parse(
            "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
             [[hook]]\ntarget=\"train_list_filter\"\nhandler=\"train_list\"\nmode=\"add\"\n",
        )
        .unwrap();
        assert!(m.validate().unwrap_err().to_string().contains("requires `list`"));
    }

    #[test]
    fn unknown_fields_refuse_the_mod() {
        let bad = GOOD.to_owned() + "\n[[effect]]\nkind = \"x\"\n";
        let err = parse(&bad).unwrap_err().to_string();
        assert!(err.contains("unknown field"), "{err}");
        // [native] (mods that ship code) lands post-M4 — unknown field here
        // (the scan path adds the milestone pointer)
        let bad = GOOD.to_owned() + "\n[native]\nabi = 1\n";
        let err = parse(&bad).unwrap_err().to_string();
        assert!(err.contains("unknown field `native`"), "{err}");
        // the M3 sections are supported now and must parse
        let m3 = GOOD.to_owned()
            + "\n[[pack]]\npack = \"mtentr0\"\n\n[[pack.entry]]\nname = \"_d_x.dds\"\nwidth = 1\nheight = 2\nfile = \"e/x.dds\"\n"
            + "\n[[csv]]\nname = \"train_form.csv\"\nfile = \"csv/train_form.csv\"\n"
            + "\n[[file]]\ndest = \"data/texture/x.dds\"\nfile = \"files/x.dds\"\n";
        let m = parse(&m3).unwrap();
        m.validate().unwrap();
        assert_eq!(m.pack.len(), 1);
        assert_eq!(m.pack[0].entry.len(), 1);
        assert_eq!(m.pack[0].entry[0].width, 1);
        assert_eq!(m.csv.len(), 1);
        assert_eq!(m.file.len(), 1);
    }

    #[test]
    fn m3_section_validation_rejects_the_expected_classes() {
        // csvp as a [[pack]] name
        let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[pack]]\npack=\"csvp\"\n[[pack.entry]]\nname=\"t.csv\"\nwidth=1\nheight=1\nfile=\"f\"\n").unwrap();
        let e = m.validate().unwrap_err().to_string();
        assert!(e.contains("[[csv]]"), "{e}");
        // duplicate pack sections
        let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[pack]]\npack=\"me0\"\n[[pack.entry]]\nname=\"a\"\nwidth=1\nheight=1\nfile=\"f\"\n[[pack]]\npack=\"me0\"\n[[pack.entry]]\nname=\"b\"\nwidth=1\nheight=1\nfile=\"f\"\n").unwrap();
        let e = m.validate().unwrap_err().to_string();
        assert!(e.contains("merge their entries"), "{e}");
        // duplicate entry names in one pack
        let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[pack]]\npack=\"me0\"\n[[pack.entry]]\nname=\"a\"\nwidth=1\nheight=1\nfile=\"f\"\n[[pack.entry]]\nname=\"a\"\nwidth=1\nheight=1\nfile=\"f\"\n").unwrap();
        let e = m.validate().unwrap_err().to_string();
        assert!(e.contains("two entries named"), "{e}");
        // missing file / empty
        let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[pack]]\npack=\"me0\"\n[[pack.entry]]\nname=\"a\"\nwidth=1\nheight=1\nfile=\" \"\n").unwrap();
        assert!(m.validate().unwrap_err().to_string().contains("no `file`"));
        // uppercase pack name
        let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[pack]]\npack=\"Me0\"\n[[pack.entry]]\nname=\"a\"\nwidth=1\nheight=1\nfile=\"f\"\n").unwrap();
        assert!(m.validate().unwrap_err().to_string().contains("slug"));
        // duplicate csv tables
        let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[csv]]\nname=\"t.csv\"\nfile=\"f\"\n[[csv]]\nname=\"t.csv\"\nfile=\"g\"\n").unwrap();
        let e = m.validate().unwrap_err().to_string();
        assert!(e.contains("two [[csv]]"), "{e}");
        // csv mode: `rows` parses + validates; `mode` defaults to replace
        let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[csv]]\nname=\"t.csv\"\nfile=\"f\"\nmode=\"rows\"\n").unwrap();
        m.validate().unwrap();
        assert_eq!(m.csv[0].mode, CsvMode::Rows);
        assert!(m.csv[0].row_keys.is_empty());
        let d = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[csv]]\nname=\"t.csv\"\nfile=\"f\"\n").unwrap();
        assert_eq!(d.csv[0].mode, CsvMode::Replace);
        // an unknown mode is a hard refusal (deny_unknown_fields + enum)
        assert!(parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[csv]]\nname=\"t.csv\"\nfile=\"f\"\nmode=\"cells\"\n").is_err());
        // `row_keys` is derived state — authoring it is refused (skipped field)
        assert!(parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[csv]]\nname=\"t.csv\"\nfile=\"f\"\nrow_keys=[\"R1\"]\n").is_err());
        // file dest escaping the game dir (TOML needs \\ for a literal backslash)
        for dest in ["/abs/x.dds", "C:\\\\x.dds", "data/../../x.dds"] {
            let m = parse(&format!("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[file]]\ndest=\"{dest}\"\nfile=\"f\"\n")).unwrap();
            assert!(m.validate().is_err(), "dest {dest} must be refused");
        }
        // duplicate file dests
        let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[file]]\ndest=\"data/x.dds\"\nfile=\"f\"\n[[file]]\ndest=\"data/x.dds\"\nfile=\"g\"\n").unwrap();
        assert!(m.validate().unwrap_err().to_string().contains("two [[file]]"));
    }

    #[test]
    fn m5_asset_section_parses_and_validates() {
        // a minimal valid new asset, hosted in vp
        let m = parse(
            "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
             [[asset]]\nname=\"_t_mynew_a.x\"\ndir=\"train\\\\mynew\"\nhost=\"vp\"\nfile=\"assets/_t_mynew_a.x\"\n",
        )
        .unwrap();
        m.validate().unwrap();
        assert_eq!(m.asset.len(), 1);
        assert_eq!(m.asset[0].name, "_t_mynew_a.x");
        assert_eq!(m.asset[0].dir, "train\\mynew");
        assert_eq!(m.asset[0].host, "vp");
        assert!(!m.asset[0].override_vanilla, "default is false");
        // override_vanilla round-trips
        let ov = parse(
            "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
             [[asset]]\nname=\"_t_van_a.x\"\ndir=\"train\\\\van\"\nhost=\"tp\"\nfile=\"f\"\noverride_vanilla=true\n",
        )
        .unwrap();
        ov.validate().unwrap();
        assert!(ov.asset[0].override_vanilla);

        // every family-A host classifies; a family-B/unknown host is refused
        for host in ["tp", "mcp", "ep", "cp"] {
            let m = parse(&format!(
                "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
                 [[asset]]\nname=\"_t_a.x\"\ndir=\"d\"\nhost=\"{host}\"\nfile=\"f\"\n"
            ))
            .unwrap();
            m.validate().unwrap();
        }
        for host in ["i.dat", "mtentr0", "me3", "csvp", "bogus", "VP"] {
            let m = parse(&format!(
                "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
                 [[asset]]\nname=\"_t_a.x\"\ndir=\"d\"\nhost=\"{host}\"\nfile=\"f\"\n"
            ))
            .unwrap();
            let e = m.validate().unwrap_err().to_string();
            assert!(e.contains("family-A pack"), "host {host}: {e}");
        }
        // duplicate asset names in one mod
        let m = parse(
            "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
             [[asset]]\nname=\"_t_a.x\"\ndir=\"d\"\nhost=\"vp\"\nfile=\"f\"\n\
             [[asset]]\nname=\"_t_a.x\"\ndir=\"d\"\nhost=\"vp\"\nfile=\"g\"\n",
        )
        .unwrap();
        let e = m.validate().unwrap_err().to_string();
        assert!(e.contains("two [[asset]]"), "{e}");
        // empty name / empty file
        let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[asset]]\nname=\" \"\ndir=\"d\"\nhost=\"vp\"\nfile=\"f\"\n").unwrap();
        assert!(m.validate().unwrap_err().to_string().contains("nonempty plain name"));
        let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[asset]]\nname=\"a.x\"\ndir=\"d\"\nhost=\"vp\"\nfile=\" \"\n").unwrap();
        assert!(m.validate().unwrap_err().to_string().contains("no `file`"));
        // unknown field inside the section is refused (strict schema)
        assert!(parse(
            "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
             [[asset]]\nname=\"a.x\"\ndir=\"d\"\nhost=\"vp\"\nfile=\"f\"\nwidth=1\n"
        )
        .is_err());
    }

    #[test]
    fn m5b_asset_folder_form_parses_and_validates() {
        // folder form: name/file absent, dir optional, host defaults to auto
        let m = parse(
            "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
             [[asset]]\nfolder=\"assets\"\n",
        )
        .unwrap();
        m.validate().unwrap();
        assert_eq!(m.asset[0].folder.as_deref(), Some("assets"));
        assert_eq!(m.asset[0].host, HOST_AUTO, "folder form defaults host to auto");
        assert!(m.asset[0].name.is_empty() && m.asset[0].file.is_empty());

        // host = "auto" is allowed explicitly in the single-file form too,
        // and is the default when `host` is omitted
        let m = parse(
            "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
             [[asset]]\nname=\"_t_a.x\"\ndir=\"train\\\\a\"\nhost=\"auto\"\nfile=\"f\"\n\
             [[asset]]\nname=\"_t_b.x\"\ndir=\"train\\\\b\"\nfile=\"f\"\n",
        )
        .unwrap();
        m.validate().unwrap();
        assert_eq!(m.asset[0].host, HOST_AUTO);
        assert_eq!(m.asset[1].host, HOST_AUTO, "omitted host defaults to auto");

        // exactly one form: name/file with folder is refused; neither is refused
        let both = parse(
            "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
             [[asset]]\nfolder=\"assets\"\nname=\"_t_a.x\"\nfile=\"f\"\n",
        )
        .unwrap();
        assert!(both.validate().unwrap_err().to_string().contains("not allowed together"));
        let neither = parse(
            "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[asset]]\ndir=\"d\"\n",
        )
        .unwrap();
        assert!(neither.validate().unwrap_err().to_string().contains("nonempty plain name"));
        // folder paths escaping the mod dir are refused
        for folder in ["/abs", "..\\\\x", "C:\\\\x"] {
            let m = parse(&format!(
                "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[asset]]\nfolder=\"{folder}\"\n"
            ))
            .unwrap();
            assert!(m.validate().is_err(), "folder {folder} must be refused");
        }
        // the removed empty-name message stays: a bare name is required in the
        // single-file form (folder mode is the way to avoid naming rows)
        let m = parse("\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n[[asset]]\nname=\" \"\nhost=\"vp\"\nfile=\"f\"\n").unwrap();
        assert!(m.validate().unwrap_err().to_string().contains("nonempty plain name"));
        // unknown field stays refused
        assert!(parse(
            "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
             [[asset]]\nfolder=\"assets\"\nrecurse=false\n"
        )
        .is_err());
    }

    #[test]
    fn validation_rejects_the_expected_classes() {
        // raw patch without expect fails AT PARSE (the Raw target requires it)
        let raw_no_expect = GOOD.replace(
            "target = \"train_list_whitelist\"",
            "target = { rva = 0x6FC1C0 }",
        );
        assert!(parse(&raw_no_expect).is_err());
        // raw target with empty expect fails validation
        let raw_empty = r#"
id = "diag-table113"
name = "Diagnostic table113"
version = "1.0.0"
author = "t"

[[patch]]
target = { rva = 0x6FC1C0, expect = "" }
offset = 24
data = "88 00 00 00"
"#;
        let m = parse(raw_empty).unwrap();
        let raw_expect_err = m.validate().unwrap_err().to_string();
        assert!(raw_expect_err.contains("non-empty `expect`"), "{raw_expect_err}");
        // bad hex data
        let m = parse(&GOOD.replace("88 00 00 00", "zz")).unwrap();
        let e = m.validate().unwrap_err().to_string();
        assert!(e.contains("non-empty valid hex"), "{e}");
        // unknown handler
        let m = parse(&GOOD.replace("handler = \"train_list\"", "handler = \"trainlst\"")).unwrap();
        let e = m.validate().unwrap_err().to_string();
        assert!(e.contains("unknown handler"), "{e}");
        // train_list without list
        let m = parse(&GOOD.replace("list = \"liveries.txt\"", "")).unwrap();
        let e = m.validate().unwrap_err().to_string();
        assert!(e.contains("requires `list`"), "{e}");
        // nop with params
        let m = parse(&GOOD.replace("handler = \"train_list\"", "handler = \"nop\"")).unwrap();
        let e = m.validate().unwrap_err().to_string();
        assert!(e.contains("no parameters"), "{e}");
        // bad id
        let m = parse(&GOOD.replace("id = \"jp-liveries\"", "id = \"JP Liveries\"")).unwrap();
        let e = m.validate().unwrap_err().to_string();
        assert!(e.contains("slug"), "{e}");
        // self-conflict
        let m = parse(&GOOD.replace("\"stock-liveries\"", "\"jp-liveries\"")).unwrap();
        let e = m.validate().unwrap_err().to_string();
        assert!(e.contains("itself"), "{e}");
        // duplicate target within one mod
        let dup = GOOD.to_owned()
            + "\n[[patch]]\ntarget = \"train_list_whitelist\"\noffset = 24\ndata = \"01\"\nexpect = \"bb\"\n";
        let m = parse(&dup).unwrap();
        let e = m.validate().unwrap_err().to_string();
        assert!(e.contains("same target"), "{e}");
    }

    #[test]
    fn raw_target_repr() {
        let m = parse(
            "\nid=\"x\"\nname=\"X\"\nversion=\"1.0.0\"\nauthor=\"a\"\n\
             [[patch]]\ntarget = { rva = 0x1A13B0, expect = \"48,89\" }\ndata = \"c3\"\n",
        )
        .unwrap();
        m.validate().unwrap();
        assert_eq!(m.patch[0].target.key(0), "raw:0x1a13b0+0");
        assert!(m.patch[0].target.anchor().is_none());
    }
}
