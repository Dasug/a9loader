//! `[[train]]` — the M7-2 train compiler (pure; no Win32, no game memory).
//!
//! PLAN §M7: a mod can add brand-new train types beyond the 417-name FORM
//! catalog without hand-picking a slot. This module is the *compiler*: it turns
//! a [`TrainSection`] declaration into a [`CompiledTrain`] — an allocated spec
//! row id + FORM name, the four derived `csvp` rows, the three preview-icon
//! assets, the visibility entry, and the spec-row injection request. The loader
//! expands that into the effects that already exist (row merges, asset entries,
//! the row injection); nothing here touches the game.
//!
//! # Identity allocation
//!
//! The author never writes an id. The FORM name is
//! `FORM_<MODID>_<KEY>` (upper-cased, every non-alphanumeric folded to `_`) —
//! unique per mod and filename-safe, so the preview annotator's
//! `<name-lowercased>_<frame>.dds` composition yields the icon names directly
//! (`FORM_DUMMY_02` -> `form_dummy_02_f.dds`). The spec-row id is
//! `base_id + index` where `base_id`
//! is the boot's first free row (421: `spec_table_resize` pins the vector to
//! exactly 421 rows before the injection) and `index` is the train's position
//! in the resolved boot order. Ids must stay below [`FORMS_MAX`]
//! ([`crate::manifest::`]-independent; the loader's keep-bitmap width) and
//! within a `u16` (the save stores per-car ids as u16); the compiler refuses
//! loudly rather than wrapping.
//!
//! # The keep-bitmap
//!
//! The composed purchase filter matches declared **names** against the FORM
//! list, and an allocated name is deliberately not in that list — so the
//! loader must add the reserved **ids** to the bitmap programmatically. This
//! module exposes [`CompiledTrain::id`] for exactly that; the loader does it.
//!
//! # The derived rows
//!
//! The four `csvp` tables are derived from the declaration plus the donor
//! profile (this version reuses a shipped model, so the profile columns —
//! vehicle type, company, acceleration class, the four sound classes and the
//! two driving modes — carry the donor's shipped values as documented
//! defaults). `train_form` col1 `有効` is `ON` and col16 `特殊車両フラグ` is
//! empty: both are invalidation traps (anything but `ON`, or a non-zero special
//! flag, invalidates the row). `train_pattern` is a whole 10-row group + a
//! separator, which is why the loader needs the positional group-append
//! primitive: only the head row carries the key.

use crate::manifest::{Manifest, TrainModel, TrainSection};
use crate::ModError;
use a9_formats::csv::Table;

/// The spec vector is pinned to exactly this many rows before the train
/// injection (`spec_table_resize` 0x14019f150, `mov ebx,0x1a5`), so the first
/// allocatable id is this value.
pub const BASE_ID: usize = 421;
/// The composed keep-bitmap width ([`crate::forms`] in the loader) — an id at
/// or above it can never be admitted by the filter.
pub const FORMS_MAX: usize = 1024;
/// The per-car save id is a `u16`.
pub const U16_MAX: usize = 65535;

/// The donor's shipped profile defaults (KiHa-32 based; the new-train template
/// uses exactly these). A future version that reads the shipped table would
/// derive them from `from` instead.
const DEFAULT_TYPE: &str = "旅客列車";
const DEFAULT_COMPANY: &str = "ＪＲ四国";
const DEFAULT_ACCEL: &str = "低";
const DEFAULT_SOUNDS: [&str; 4] = ["低速車両", "低速車両", "低速車両_貨物警笛", "低速車両"];
const DEFAULT_DRIVING: &str = "ディーゼル車";

/// The default donor FORM name.
///
/// It must be a train this build actually ENABLES: the spec row of a shipped
/// train that the game never populates stays `bValid == 0` for the whole boot, so
/// the injection refuses rather than cloning an uninitialised row. That is not a
/// timing question — the live probe showed `FORM_KH32` (index 195) still invalid
/// at a call made long after the boot, while `FORM_AR4` is in the shipped
/// purchase whitelist and is therefore live in every version.
///
/// `kh32` remains the default MODEL (`DEFAULT_DIR`/`DEFAULT_MESH`): the mesh and
/// its liveries are shipped even though the train itself is not enabled here.
pub const DEFAULT_FROM: &str = "FORM_AR4";
const DEFAULT_DIR: &str = "kh32";
const DEFAULT_MESH: &str = "_t_kh32_a";
const DEFAULT_PRICE: u64 = 45_000_000;
const DEFAULT_CAPACITY: u32 = 106;
const DEFAULT_RUNNING_COST: u32 = 26_200;
const DEFAULT_CRUISE: u16 = 65;
const DEFAULT_FARE: u16 = 300;

/// The `train_pattern` group is 10 rows + a separator; the head is row 1.
const PATTERN_GROUP_ROWS: usize = 10;

/// The four derived `csvp` rows of one train. `form`/`comment`/`custom` are one
/// row each; `pattern` is the whole 10-row group plus its separator (the
/// positional append payload).
#[derive(Debug, Clone, PartialEq)]
pub struct TrainRows {
    pub form: Vec<String>,
    pub pattern: Vec<Vec<String>>,
    pub comment: Vec<String>,
    pub custom: Vec<String>,
}

/// One preview-icon asset: the derived i.dat row name and the mod-relative
/// payload file the author ships (`assets/<name>` by convention).
#[derive(Debug, Clone, PartialEq)]
pub struct IconAsset {
    pub name: String,
    pub file: String,
}

/// One compiled `[[train]]`: everything the loader needs to expand it.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledTrain {
    pub mod_id: String,
    pub key: String,
    /// the allocated FORM name (`FORM_<MODID>_<KEY>`)
    pub form_name: String,
    /// the lower-cased FORM name — the preview-icon stem
    pub token: String,
    /// the allocated spec-row id
    pub id: usize,
    /// the donor shipped FORM name whose row is cloned
    pub donor: String,
    pub name_jp: String,
    pub name_en: String,
    /// the runtime livery name (default = the model dir): the shipped
    /// `_d_<livery>.dds` the reused model renders with. Documentation only in
    /// this version — a new livery needs the M5 `[[asset]]` route.
    pub livery: String,
    pub cruise_speed: u16,
    pub reverse_speed: Option<u16>,
    pub fare: u16,
    pub cars: u8,
    pub rows: TrainRows,
    pub icons: Vec<IconAsset>,
}

impl CompiledTrain {
    /// The overlap/priority keys a `[[train]]` produces — the exact effect keys
    /// the expansion carries, so `resolve` composes them like any other effect.
    ///
    /// * `csv:train_form.csv:<form>` / `csv:train_pattern.csv:<form>` /
    ///   `csv:train_comment.csv:<form>` / `csv:train_custom.csv:<form>` — the
    ///   four row effects (the pattern one is the positional group append);
    /// * `asset:<icon>` ×3 — the preview-icon rows;
    /// * `hook:train_list_filter` — the visibility entry (composable, like any
    ///   `train_list` contribution);
    /// * `train:<form>` — the spec-row injection (its own namespace).
    pub fn effect_keys(&self) -> Vec<String> {
        let mut keys = vec![
            format!("csv:train_form.csv:{}", self.form_name),
            format!("csv:train_pattern.csv:{}", self.form_name),
            format!("csv:train_comment.csv:{}", self.form_name),
            format!("csv:train_custom.csv:{}", self.form_name),
            "hook:train_list_filter".to_string(),
            format!("train:{}", self.form_name),
        ];
        for i in &self.icons {
            keys.push(format!("asset:{}", i.name));
        }
        keys
    }

    /// The `mode = "rows"` payload for one of the three single-row tables.
    pub fn form_payload(&self) -> Vec<u8> {
        fields_to_csv(std::slice::from_ref(&self.rows.form))
    }

    /// The `mode = "rows"` payload for `train_comment`.
    pub fn comment_payload(&self) -> Vec<u8> {
        fields_to_csv(std::slice::from_ref(&self.rows.comment))
    }

    /// The `mode = "rows"` payload for `train_custom`.
    pub fn custom_payload(&self) -> Vec<u8> {
        fields_to_csv(std::slice::from_ref(&self.rows.custom))
    }

    /// The positional group-append payload for `train_pattern`.
    pub fn pattern_payload(&self) -> Vec<u8> {
        fields_to_csv(&self.rows.pattern)
    }
}

/// Serialize row field-vectors as a canonical UTF-16LE CSV payload (BOM, CRLF,
/// QUOTE_ALL) — the same shape the `[[csv]] mode = "rows"` merge produces.
fn fields_to_csv(rows: &[Vec<String>]) -> Vec<u8> {
    let mut t = Table::new();
    for r in rows {
        t.push_row(r.clone());
    }
    t.serialize()
}

/// Fold a mod id or key into the FORM-name alphabet: upper-case ASCII
/// alphanumerics, everything else to `_`.
fn fold_upper(s: &str) -> String {
    s.chars()
        .map(|c| {
            let u = c.to_ascii_uppercase();
            if u.is_ascii_alphanumeric() {
                u
            } else {
                '_'
            }
        })
        .collect()
}

/// The allocated FORM name for a train: `FORM_<MODID>_<KEY>`, upper-cased.
pub fn form_name(mod_id: &str, key: &str) -> String {
    format!("FORM_{}_{}", fold_upper(mod_id), fold_upper(key))
}

/// A slug check for `[[train]] key`.
fn is_key_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 24
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A plain, safe model dir / mesh (no path separators, no NUL).
fn is_plain_name(s: &str) -> bool {
    !s.trim().is_empty()
        && !s.contains('\0')
        && !s.contains('/')
        && !s.contains('\\')
        && !s.contains(':')
        && !s.contains(' ')
}

/// A donor FORM name (letters/digits/underscore, non-empty).
fn is_form_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Compile every `[[train]]` of one mod, allocating ids from `base_id` upward.
/// `base_id` is the first free spec-row id at boot (normally [`BASE_ID`], plus
/// whatever earlier mods in the resolved order already allocated).
///
/// Per-train fail-open: a malformed train is **skipped loudly** (its message is
/// returned in the second vector) and the mod's other trains — and every other
/// mod — still load. Ids are allocated by index, so a skipped train leaves an
/// invisible gap (every list surface already skips `row[0] == 0`).
pub fn compile(
    mod_id: &str,
    trains: &[TrainSection],
    base_id: usize,
) -> (Vec<CompiledTrain>, Vec<String>) {
    let mut out = Vec::with_capacity(trains.len());
    let mut refused = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (i, t) in trains.iter().enumerate() {
        match compile_one(mod_id, t, base_id + i, &mut seen) {
            Ok(c) => out.push(c),
            Err(e) => refused.push(format!("train '{}' (#{}): {e}", t.key, i + 1)),
        }
    }
    (out, refused)
}

/// Compile one `[[train]]`. `seen` carries the FORM names already allocated in
/// this mod (duplicate detection).
fn compile_one(
    mod_id: &str,
    t: &TrainSection,
    id: usize,
    seen: &mut std::collections::BTreeSet<String>,
) -> Result<CompiledTrain, ModError> {
    if id >= FORMS_MAX {
        return Err(ModError::Invalid(format!(
            "allocated id {id} >= the composed keep-set width {FORMS_MAX} — the \
             loader could never admit it; too many new trains"
        )));
    }
    if id > U16_MAX {
        return Err(ModError::Invalid(format!(
            "allocated id {id} exceeds the u16 save-id ceiling"
        )));
    }
    if !is_key_slug(&t.key) {
        return Err(ModError::Invalid(format!(
            "`key` must be a lowercase slug [a-z0-9_]+ (<=24 chars), got {:?}",
            t.key
        )));
    }
    if t.name.trim().is_empty() || t.name.contains('\0') {
        return Err(ModError::Invalid(
            "`name` must be non-empty and NUL-free".into(),
        ));
    }
    let form = form_name(mod_id, &t.key);
    if !seen.insert(form.clone()) {
        return Err(ModError::Invalid(format!(
            "the allocated FORM name {form} is already used by another [[train]] of \
             this mod — keys must be unique"
        )));
    }
    let donor = t.from.clone().unwrap_or_else(|| DEFAULT_FROM.to_string());
    if !is_form_name(&donor) {
        return Err(ModError::Invalid(format!(
            "`from` must be a FORM name (letters/digits/underscore), got {donor:?}"
        )));
    }
    let (dir, mesh) = match &t.model {
        Some(TrainModel { dir, mesh }) => (dir.clone(), mesh.clone()),
        None => (DEFAULT_DIR.to_string(), DEFAULT_MESH.to_string()),
    };
    if !is_plain_name(&dir) || !is_plain_name(&mesh) {
        return Err(ModError::Invalid(format!(
            "model dir/mesh must be plain names (no path separators), got dir {dir:?} \
             mesh {mesh:?}"
        )));
    }
    let cars = t.cars.unwrap_or(1);
    if cars != 1 {
        return Err(ModError::Invalid(format!(
            "`cars = {cars}` — only 1 is supported in this version (a multi-car \
             consist needs several distinct car models)"
        )));
    }
    let cruise = t.cruise_speed.unwrap_or(DEFAULT_CRUISE);
    if cruise > 360 {
        return Err(ModError::Invalid(format!(
            "`cruise_speed = {cruise}` exceeds the engine's 360 cap"
        )));
    }
    let livery = t
        .livery
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| dir.clone());
    if !is_plain_name(&livery) {
        return Err(ModError::Invalid(format!(
            "`livery` must be a plain name (no path separators), got {livery:?}"
        )));
    }
    let name_en = t
        .name_en
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| t.name.clone());
    let token = form.to_ascii_lowercase();
    let icons = ["f", "s", "s_1"]
        .iter()
        .map(|suffix| {
            // the annotator composes `<lowercased FORM name>_<frame>.dds`, so the
            // allocated name's lowercase IS the icon stem (FORM_DUMMY_02 ->
            // form_dummy_02_f.dds)
            let name = format!("{token}_{suffix}.dds");
            IconAsset {
                file: format!("assets/{name}"),
                name,
            }
        })
        .collect();

    let rows = derive_rows(
        &form,
        &t.name,
        &name_en,
        &dir,
        &mesh,
        t.price.unwrap_or(DEFAULT_PRICE),
        t.capacity.unwrap_or(DEFAULT_CAPACITY),
        t.running_cost.unwrap_or(DEFAULT_RUNNING_COST),
        cruise,
        t.reverse_speed,
        t.fare.unwrap_or(DEFAULT_FARE),
    );

    Ok(CompiledTrain {
        mod_id: mod_id.to_string(),
        key: t.key.clone(),
        form_name: form,
        token,
        id,
        donor,
        name_jp: t.name.clone(),
        name_en,
        livery,
        cruise_speed: cruise,
        reverse_speed: t.reverse_speed,
        fare: t.fare.unwrap_or(DEFAULT_FARE),
        cars,
        rows,
        icons,
    })
}

/// The effect keys of every valid `[[train]]` of one mod — for the resolver's
/// overlap registration. Uses [`BASE_ID`] internally (the keys carry no ids).
pub fn effect_keys_of(mod_id: &str, trains: &[TrainSection]) -> Vec<String> {
    let (c, _) = compile(mod_id, trains, BASE_ID);
    c.iter().flat_map(|t| t.effect_keys()).collect()
}

/// Compile every enabled mod's trains in resolved order, threading the id base
/// across mods so allocations never collide. Returns the compiled trains and
/// one refusal line per skipped train.
pub fn compile_all(
    manifests: &[Manifest],
    order: &[usize],
) -> (Vec<CompiledTrain>, Vec<String>) {
    let mut out = Vec::new();
    let mut refused = Vec::new();
    let mut base = BASE_ID;
    for &i in order {
        let (c, r) = compile(&manifests[i].id, &manifests[i].train, base);
        base += manifests[i].train.len(); // keep ids stable across skips
        out.extend(c);
        refused.extend(r.into_iter().map(|m| format!("mod '{}': {m}", manifests[i].id)));
    }
    (out, refused)
}

/// Build the four derived rows.
#[allow(clippy::too_many_arguments)]
fn derive_rows(
    form: &str,
    name_jp: &str,
    name_en: &str,
    dir: &str,
    mesh: &str,
    price: u64,
    capacity: u32,
    running_cost: u32,
    cruise: u16,
    reverse: Option<u16>,
    fare: u16,
) -> TrainRows {
    // train_form.csv: 29 columns (19 parsed).
    let mut form_row: Vec<String> = vec![
        form.into(),                                    // 0 編成ＩＤ
        "ON".into(),                                    // 1 有効 (trap)
        DEFAULT_TYPE.into(),                            // 2 車両種別ID
        DEFAULT_COMPANY.into(),                         // 3 鉄道会社ID
        cruise.to_string(),                             // 4 巡航速度
        reverse.map(|v| v.to_string()).unwrap_or_default(), // 5 反転速度
        DEFAULT_ACCEL.into(),                           // 6 加速
        fare.to_string(),                               // 7 基本運賃
        DEFAULT_SOUNDS[0].into(),                       // 8 発進音ID
        DEFAULT_SOUNDS[1].into(),                       // 9 駆動音ID
        DEFAULT_SOUNDS[2].into(),                       // 10 警笛音ID
        DEFAULT_SOUNDS[3].into(),                       // 11 ブレーキ音ID
        "0".into(),                                     // 12 プレビューx
        "0".into(),                                     // 13 プレビューy
        "0".into(),                                     // 14 2Dプレビューx
        "1".into(),                                     // 15 2Dプレビューy
        String::new(),                                  // 16 特殊車両フラグ (trap: empty)
        DEFAULT_DRIVING.into(),                         // 17 運転モード警笛
        DEFAULT_DRIVING.into(),                         // 18 運転モード加減速
        name_jp.into(),                                 // 19 コメント
    ];
    form_row.resize(29, String::new()); // 20..28 備忘録

    // train_pattern.csv: a 10-row group + a separator (18 columns each).
    let mut pattern: Vec<Vec<String>> = Vec::with_capacity(PATTERN_GROUP_ROWS + 1);
    let mut head = vec![String::new(); 18];
    head[0] = form.into(); // 編成ＩＤ (the only keyed row)
    head[1] = dir.into(); // ディレクトリ
    head[2] = mesh.into(); // モデル
    head[3] = capacity.to_string(); // 定員積載
    head[4] = running_cost.to_string(); // 運行費用
    head[5] = price.to_string(); // 購入価格
    head[6] = "1".into(); // the 1-car consist template
    head[16] = "○".into(); // 標準
    head[17] = "1".into(); // the row's index within the group
    pattern.push(head);
    for n in 2..=PATTERN_GROUP_ROWS {
        let mut row = vec![String::new(); 18];
        row[17] = n.to_string();
        pattern.push(row);
    }
    pattern.push(vec![String::new(); 18]); // the separator

    // train_comment.csv: 11 columns.
    let mut comment = vec![String::new(); 11];
    comment[0] = form.into(); // 編成ID
    comment[1] = name_jp.into(); // 日本語
    comment[2] = name_en.into(); // 英語

    // train_custom.csv: 19 columns (coupling / pantograph constraints).
    let mut custom = vec![String::new(); 19];
    custom[0] = form.into();
    custom[3] = "1".into(); // パンタ不可 (a diesel railcar has no pantograph)
    custom[4] = "0".into(); // A前
    custom[5] = "0".into(); // A後
    custom[18] = name_jp.into();

    TrainRows {
        form: form_row,
        pattern,
        comment,
        custom,
    }
}

#[cfg(test)]
mod tests;
