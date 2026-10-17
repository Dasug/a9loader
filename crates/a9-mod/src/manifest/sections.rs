//! The manifest sections — `[[pack]]`, `[[csv]]`, `[[asset]]` and
//! `[[loose_asset]]` — with their serde shapes and per-field findings. The M2
//! core sections (`[[patch]]`/`[[hook]]`) and the root
//! [`Manifest`](super::Manifest) stay in [`super`].

use serde::{Deserialize, Serialize};

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

pub(super) fn default_host() -> String {
    HOST_AUTO.to_string()
}

/// `[[asset]]` — a genuinely-new i.dat row whose payload is appended to a
/// family-A host pack's decoded stream. Served only through the merge
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

/// `[[loose_asset]]` — a game file the game opens BY PATH (not content inside
/// a pack): the loader serves the mod's bytes in place of the file on disk.
///
/// Motivating case: the game builds its bitmap-font atlases at every boot,
/// writes them to a PER-BOOT RANDOM temp directory
/// (`%TEMP%\<random>.adk\fe01.bin` … `fs02.bin`), then reads them back through
/// the same `CFile::Open` funnel the overlay hooks — so the match must be a
/// PATTERN, never an exact path. An exact game path (`data\bmf\fy01.bin`)
/// works too (a `match` with no `*` is an exact, case-insensitive path).
///
/// The loader materializes each payload into `<mods_dir>/cache/loose/` (a real
/// file — `CFile::Read`/`GetLength` are plain Win32 on the handle, so there is
/// no memory-source branch to exploit) and rewrites the game's open path to it.
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct LooseAssetSection {
    /// case-insensitive GLOB over the game's own open path (backslashes
    /// normalized; `*` = any run, including the empty run). `?` and `[`/`]`
    /// are ordinary characters. A `match` with no `*` is an exact path.
    #[serde(rename = "match")]
    pub match_: String,
    /// the payload, relative to the mod dir (must not escape it)
    pub file: String,
}

/// `[[train]]` — a brand-new train type, the one section that adds
/// trains BEYOND the 417-name FORM catalog without the author hand-picking a
/// slot.
///
/// It is a **compiler over primitives**: the author declares the train's own
/// values, the loader allocates the row id and FORM name, derives the four
/// `csvp` rows (`train_form`, `train_pattern`, `train_comment`,
/// `train_custom`), the three preview-icon assets and the `train_list`
/// visibility entry, and expands all of it into the effects that already
/// exist — so overlap keys, priority and conflict handling compose unchanged.
/// The loader also injects the matching spec row (a clone of a shipped donor
/// row, renamed and re-statted) at boot.
///
/// The author **never** writes a form id, a slot name or a CSV row. The
/// allocated FORM name is `FORM_<MODID>_<KEY>` (upper-cased, non-alphanumerics
/// folded to `_`), and the preview-icon stem is that name lower-cased.
///
/// Limitations of this version (all loud, fail-open):
/// * `cars` must be 1 — a multi-car consist needs several distinct car models,
///   which this version does not author;
/// * the four derived rows are keyed by the allocated name, which is not in the
///   FORM literal, so the CSV pipeline does not attach them to the injected
///   row; the row's runtime data comes from the donor clone + the declared
///   overrides (price/capacity/running cost live only in `train_pattern` and
///   are therefore not applied to the row).
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct TrainSection {
    /// short slug identifying this train within the mod
    /// (`[a-z0-9_]{1,24}`); it derives the FORM name and the icon stem, so it
    /// must be filename-safe and unique per mod.
    pub key: String,
    /// the train's display name (the `train_comment` 日本語 column and the
    /// donor row's JP display lane).
    pub name: String,
    /// the English display name (`train_comment` 英語; defaults to `name`).
    #[serde(default)]
    pub name_en: Option<String>,
    /// the shipped FORM name whose spec row is cloned (the model/livery and
    /// profile donor). Default `FORM_AR4` — an ARTDINK original that is in the
    /// shipped purchase whitelist, so its spec row is populated in every build.
    /// It must be a train the game ENABLES: a shipped-but-disabled train (such as
    /// `FORM_KH32`) keeps `bValid == 0` for the whole boot and the injection
    /// refuses rather than cloning an uninitialised row. The model defaults stay
    /// the shipped KiHa-32 ones, whose mesh and liveries are present regardless.
    #[serde(default)]
    pub from: Option<String>,
    /// purchase price (`train_pattern` 購入価格), in the SAME unit the shipped
    /// CSVs use — so a mod's number is directly comparable with a vanilla
    /// train's. The game converts for display itself (the purchase panel showed
    /// the value / 50 in one currency/language setting and / 100 in another, and
    /// the Japanese prices are yen), so the loader never rescales it.
    /// Default `45000000`.
    #[serde(default)]
    pub price: Option<u64>,
    /// capacity (`train_pattern` 定員積載), shipped-CSV unit — the purchase
    /// panel prints it 1:1 (live: a declared 111 shows as 111). Default `106`.
    #[serde(default)]
    pub capacity: Option<u32>,
    /// running cost (`train_pattern` 運行費用), shipped-CSV unit.
    /// Default `26200`.
    #[serde(default)]
    pub running_cost: Option<u32>,
    /// cruise speed (`train_form` 巡航速度, u16). Default `65`.
    #[serde(default)]
    pub cruise_speed: Option<u16>,
    /// reverse speed (`train_form` 反転速度; omitted = empty, as most rows).
    #[serde(default)]
    pub reverse_speed: Option<u16>,
    /// base fare (`train_form` 基本運賃, u16). Default `300`.
    #[serde(default)]
    pub fare: Option<u16>,
    /// consist length. Only `1` is supported in this version (a multi-car
    /// consist needs several distinct car models); a larger value is refused
    /// loudly and the train is skipped.
    #[serde(default)]
    pub cars: Option<u8>,
    /// the reused model (`train_pattern` ディレクトリ/モデル). Default
    /// `kh32` / `_t_kh32_a` (the donor's shipped model). The mesh must already
    /// be a shipped `train\<dir>\<mesh>.x` i.dat row — no model asset is
    /// authored.
    #[serde(default)]
    pub model: Option<TrainModel>,
    /// the runtime livery name (documentation only in this version: the livery
    /// is the donor model's shipped `_d_<dir>.dds`; a new livery needs the M5
    /// `[[asset]]` route). Default = the model dir.
    #[serde(default)]
    pub livery: Option<String>,
}

/// The reused model of a [`TrainSection`]: a shipped directory + mesh basename
/// (`train\<dir>\<mesh>.x`).
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct TrainModel {
    /// model directory (`train_pattern` column 1), e.g. "kh32"
    pub dir: String,
    /// mesh basename (`train_pattern` column 2), e.g. "_t_kh32_a"
    pub mesh: String,
}
