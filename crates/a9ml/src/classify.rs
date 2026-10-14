//! Asset-consumption classifier — "will my texture replace actually win?"
//!
//! The answer depends on WHERE the game consumes an asset during boot. The
//! model below is verified in `notes/24-m4-pack-model.md` **round 7** (plus the
//! round-7 early-consumer enumeration):
//!
//! * **Stage 1, pre-sequence** — `boot_pack_system_driver` 0x140319480:
//!   `i.dat` → the master map `DAT_144f86100`; `csdp` → CCommonShader `.vs`/`.ps`
//!   **COM shaders** (bytecode compiled at load, no buffer survives); `csvp` →
//!   the CSV reservoir `DAT_140ef7528`. So shader bytecode and CSV tables are
//!   consumed **before** the asset sequence ever runs.
//! * **Stage 2** — `pack_load_sequence_teardown` 0x14031c1c0:
//!   `tp → sdp → me0-3 → mtentr0/mtcutp0 → mcp → ep → cp → vp`, then teardown.
//!
//! In-window consumers pull their bytes out of the current pack buffer
//! immediately, so a target must be **resident before that window**:
//!
//! * `tp` — system/UI/terrain/sky textures + `.dat` blobs, read via
//!   `pack_master_map_query` + d3dx straight from pack memory (the named list
//!   in [`TP_NAMES`]); and **previews** — `preview_texture_pack_loader`
//!   0x1403b0ef0 (call 0x1403f1124, inside tp) creates D3D textures straight
//!   from pack memory and **bypasses the CTexture cache entirely**, so a late
//!   cache overwrite can NOT win.
//! * `sdp` — shader bytecode compiled to COM shaders at load.
//! * `mcp`/`ep`/`cp` — models (`.x`/`.att`) parsed/built in-window.
//! * `vp` — the train/road/auto/rail pipelines, reading the **pre-resident
//!   csvp reservoir**; there is no per-entry vp target among the shipped
//!   packs, so nothing *shipped* is labelled `early:vp` (table data is
//!   `pre-shader/pre-csv`). A **merge-hosted `[[asset]]`** in `vp` (M5 W1) IS
//!   consumed in the vp window — its payload is appended to the merged vp
//!   image and its i.dat row is read here — so it is labelled `early:vp`.
//!
//! **Late-override-viable**: ONLY the family-B image packs `me0-3`/`mtentr0`/
//! `mtcutp0`. Their entries go through `texture_manager_insert_raw` into the
//! CTexture cache (delegate `FUN_1403f0330` → `FUN_1403f03d0`, insert at
//! 0x1403f04b0), so a late pass with mode 0 wins — "last wins". Those are the
//! livery/kit textures: the suffix families `_d_`/`_m_`/`_l_`/`_n_`/`_a_`
//! (and `_a_b` volume textures). `.dir` blobs inside those packs are index
//! data, not textures.
//!
//! **Every** name pattern lives in this module, in the `const` tables directly
//! below, so extending the model is a one-place edit.
//!
//! The classifier is split by rule ([`classify_entry`] calls one predicate per
//! rule, in order) and by manifest section ([`classify_manifest`] collects one
//! helper's rows per section). The `const` tables and the boot-window model
//! are the "what consumes what" data; [`run`] is only the CLI rendering.

use a9_formats::pack::{self, Container, Family};
use a9_mod::manifest::{AssetSection, Manifest};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;

// ---------------------------------------------------------------- patterns

/// A name matcher. Names are normalized first (lowercased, `\` → `/`).
///
/// Extend the tables below with these; nothing else needs to change.
#[derive(Clone, Copy, Debug)]
pub enum Pat {
    /// the whole name equals this
    Eq(&'static str),
    /// the name starts with this
    Pre(&'static str),
    /// the name contains this
    Sub(&'static str),
    /// the name ends with this
    Suf(&'static str),
}

impl Pat {
    fn hit(&self, name: &str) -> bool {
        match self {
            Pat::Eq(p) => name == *p,
            Pat::Pre(p) => name.starts_with(p),
            Pat::Sub(p) => name.contains(p),
            Pat::Suf(p) => name.ends_with(p),
        }
    }
}

/// Family-B image-pack texture suffix families — the famb row delegate
/// `FUN_1403f0330` dispatch (`_m_`/`_l_` half-size, `_n_`, `_a_b`/`ab`
/// volume, default d3dx) plus the base `_d_` (diffuse/default). These are the
/// livery/kit textures; **only** entries in a family-B image pack matching
/// these are late-viable.
pub const TEX_SUFFIX_FAMILIES: &[Pat] = &[
    Pat::Sub("_d_"),
    Pat::Sub("_m_"),
    Pat::Sub("_l_"),
    Pat::Sub("_n_"),
    Pat::Sub("_a_"),
    Pat::Sub("_a_b"),
];

/// Preview classification keys of `preview_texture_pack_loader` 0x1403b0ef0:
/// per-name `wcsstr` on `L"form_"`, `L"_f.dds"`, `L"_w.dds"`, `L"_s.dds"`
/// (plus the `previewtexture.dir` name list itself). Previews are created
/// in-window from pack memory, bypassing the CTexture cache — a late cache
/// overwrite can NOT win; override requires injecting into the `tp` pack.
pub const PREVIEW_MARKERS: &[Pat] = &[
    Pat::Sub("previewtexture"),
    Pat::Sub("form_"),
    Pat::Suf("_f.dds"),
    Pat::Suf("_w.dds"),
    Pat::Suf("_s.dds"),
];

/// `tp` in-window names (round-7 early-consumer map §1.2): system/UI/terrain/
/// sky textures + `.dat` blobs read by `pack_loader_tp` 0x1403f0d20 via
/// `pack_master_map_query` + `d3dx_create_texture_from_memory` (0x140311000)
/// straight out of the pack buffer. Wildcards from the source table
/// (`window_*_table.tga`, `map_wall*`, `wavesbump*`, `heightmap*`,
/// `sunflare*`, `lensflar*`, `moon%02d.tga`) are `Pre` matchers.
pub const TP_NAMES: &[Pat] = &[
    Pat::Eq("env_map.dds"),
    Pat::Eq("noise.dds"),
    Pat::Eq("tonemap_table.bmp"),
    Pat::Eq("edit.dds"),
    Pat::Eq("height_map_paint_noise.dat"),
    Pat::Eq("height_map_alpha_table.tga"),
    Pat::Eq("firework3.tga"),
    Pat::Eq("keikoku.tga"),
    Pat::Eq("snow.tga"),
    Pat::Eq("steam.tga"),
    Pat::Eq("volcano.dat"),
    Pat::Eq("cloud_new.tga"),
    Pat::Eq("water.dds"),
    Pat::Eq("sun.bmp"),
    Pat::Eq("meshline.tga"),
    Pat::Eq("map_wall.tga"),
    Pat::Eq("map_wall_bump.tga"),
    Pat::Eq("wavesbump.tga"),
    Pat::Eq("heightmap.tga"),
    Pat::Eq("heightmap_bump.tga"),
    Pat::Pre("window_"),
    Pat::Pre("map_wall"),
    Pat::Pre("wavesbump"),
    Pat::Pre("heightmap"),
    Pat::Pre("height_map_"),
    Pat::Pre("sunflare"),
    Pat::Pre("lensflar"),
    Pat::Pre("moon"),
];

/// Model/mesh extensions — parsed + built in-window (mcp/ep/cp).
pub const MODEL_EXTS: &[Pat] = &[Pat::Suf(".x"), Pat::Suf(".att"), Pat::Suf(".dir")];

/// Model directory prefixes (the mcp dir sub-loaders + structure/ship paths):
/// a name beginning `<dir>/` is in-window model/geometry data.
pub const MODEL_DIRS: &[&str] = &[
    "structure",
    "ship",
    "clock",
    "arrow",
    "light",
    "cont",
    "bellows",
    "pantograph",
    "tree",
    "signal",
    "sky",
];

/// Shader bytecode extensions (compiled to COM shaders at load).
pub const SHADER_EXTS: &[Pat] = &[Pat::Suf(".vs"), Pat::Suf(".ps")];

// -------------------------------------------------------------- classes

/// Which boot window a target must be resident by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Early {
    /// `pack_loader_tp` 0x1403f0d20 (system/UI/terrain/sky textures, `.dat`,
    /// and previews — previews bypass the CTexture cache).
    Tp,
    /// `pack_loader_sdp` 0x1403f0850 (shader bytecode → COM shaders).
    Sdp,
    /// `pack_loader_image_generic` 0x1403efec0 (me0-3, mtentr0, mtcutp0): the
    /// family-B image packs, inserted into the CTexture cache during their
    /// window. A POST-BOOT overwrite is IGNORED (the materials bind during
    /// boot — verified 2026-10-19), so serve with injection or merge.
    Famb,
    /// `pack_loader_mcp/ep/cp` (models/meshes/prop-dir sets).
    Models,
    /// `pack_loader_vp` — the train/road/auto/rail pipelines, reading the
    /// pre-resident csvp reservoir and the models the merged `vp` hosts
    /// (a `[[asset]]` payload hosted in `vp` is created here).
    Vp,
}

impl Early {
    pub fn window(self) -> &'static str {
        match self {
            Early::Tp => "tp",
            Early::Sdp => "sdp",
            Early::Famb => "me/mt",
            Early::Models => "mcp/ep/cp",
            Early::Vp => "vp",
        }
    }
}

/// Consumption class of one target. The label is what a mod author reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// consumed inside a boot window; must be resident before it (served by
    /// window-time injection — `pack_serving = "inject"` — or merge residency).
    Early(Early),
    /// consumed in stage 1, before the asset sequence: shader bytecode or a
    /// CSV table (served before that load).
    Pre,
    /// consumption point not in the known set — be conservative.
    Unknown,
}

impl Class {
    pub fn label(self) -> String {
        match self {
            Class::Early(e) => format!("early:{}", e.window()),
            Class::Pre => "pre-shader/pre-csv".to_string(),
            Class::Unknown => "unknown".to_string(),
        }
    }
}

/// One classified target: `{mod, target, mechanism/pack, class, reason}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub mod_id: String,
    /// how the mod carries the target: `pack:<name>` | `csvp` | `file`
    pub mechanism: String,
    pub target: String,
    pub class: Class,
    pub reason: String,
}

impl Row {
    /// A single aligned line: `mod | class | mechanism | target | reason`.
    pub fn line(&self) -> String {
        format!(
            "{:<18} | {:<18} | {:<12} | {:<32} | {}",
            self.mod_id,
            self.class.label(),
            self.mechanism,
            self.target,
            self.reason
        )
    }

    /// Risk rank for sorting: 0 = riskiest (unknown), so ascending order shows
    /// what needs the most care first.
    fn risk(&self) -> u8 {
        match self.class {
            Class::Unknown => 0,
            Class::Early(_) => 1,
            Class::Pre => 2,
        }
    }
}

/// Per-mod roll-up used by `list`/`doctor`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub early: usize,
    pub pre: usize,
    pub unknown: usize,
    /// distinct early windows, in boot order
    pub windows: Vec<&'static str>,
}

impl Summary {
    pub fn of(rows: &[Row]) -> Summary {
        let mut s = Summary::default();
        for r in rows {
            match r.class {
                Class::Early(e) => {
                    s.early += 1;
                    let w = e.window();
                    if !s.windows.contains(&w) {
                        s.windows.push(w);
                    }
                }
                Class::Pre => s.pre += 1,
                Class::Unknown => s.unknown += 1,
            }
        }
        // boot order (this also exercises every Early variant)
        const ORDER: [Early; 5] = [Early::Tp, Early::Sdp, Early::Famb, Early::Models, Early::Vp];
        s.windows
            .sort_by_key(|w| ORDER.iter().position(|e| e.window() == *w).unwrap_or(usize::MAX));
        s
    }

    /// `early: M (windows: tp,mcp/ep/cp), pre: P, unknown: K`
    pub fn compact(&self) -> String {
        let mut out = format!("early: {}", self.early);
        if self.early > 0 {
            out.push_str(&format!(" (windows: {})", self.windows.join(",")));
        }
        out.push_str(&format!(", pre: {}, unknown: {}", self.pre, self.unknown));
        out
    }

    /// true when every target's consumption point is known (no `unknown` rows)
    pub fn all_known(&self) -> bool {
        self.unknown == 0
    }
}

// ------------------------------------------------------------- classifier

fn normalize(name: &str) -> String {
    name.to_ascii_lowercase().replace('\\', "/")
}

fn is_famb_pack(pack: &str) -> bool {
    matches!(pack::classify(pack), Some((Family::B, Container::Images)))
}

/// Classify one target in the context of the pack it rides in.
///
/// Rule order (first hit wins):
/// 1. family-B image pack **and** a texture suffix family → `early:me/mt`;
/// 2. preview markers → `early:tp` (preview bypass — needs tp injection);
/// 3. `tp` in-window name list → `early:tp`;
/// 4. `csdp` → `pre-shader/pre-csv`; `sdp` or `.vs`/`.ps` → `early:sdp`;
/// 5. model `.x`/`.att`/`.dir` or a known model dir → `early:mcp/ep/cp`;
/// 6. `.csv` (or the `csvp` pack) → `pre-shader/pre-csv`;
/// 7. otherwise → `unknown` (default: treat as early).
///
/// Note rule 1 precedes rule 2 on purpose: a livery/kit texture in a family-B
/// image pack is late-viable even when its name ends `_f.dds`/`_w.dds` (the
/// preview keys) — the preview loader only classifies the names enumerated in
/// the tp pack's `previewtexture.dir`, not every `_f.dds` in the tree.
pub fn classify_entry(pack: Option<&str>, name: &str) -> Verdict {
    let n = normalize(name);
    let key = pack.map(pack::classify_key);
    let famb = key.as_deref().map(is_famb_pack).unwrap_or(false);

    if let Some(v) = rule_famb_texture(pack, &n, famb) {
        return v;
    }
    if let Some(v) = rule_preview(&n) {
        return v;
    }
    if let Some(v) = rule_tp_names(&n) {
        return v;
    }
    if let Some(v) = rule_shaders(key.as_deref(), &n) {
        return v;
    }
    if let Some(v) = rule_models(pack, &n, famb) {
        return v;
    }
    if let Some(v) = rule_csv(key.as_deref(), &n) {
        return v;
    }
    // 7. unrecognized — be conservative
    Verdict {
        class: Class::Unknown,
        reason: "consumption point not in the known set — treat conservatively \
                     (window residency)".into(),
    }
}

/// Rule 1: livery/kit textures in a family-B image pack — served during the
/// pack's boot window (me/mt), inserted into the CTexture cache then.
fn rule_famb_texture(pack: Option<&str>, n: &str, famb: bool) -> Option<Verdict> {
    if famb && TEX_SUFFIX_FAMILIES.iter().any(|p| p.hit(n)) {
        return Some(Verdict {
            class: Class::Early(Early::Famb),
            reason: format!(
                "family-B image pack {} entry: inserted into the CTexture cache during \
                 the pack's boot window; a post-boot overwrite is IGNORED (the materials \
                 bind during boot) — serve with pack_serving=inject or merge",
                pack.unwrap_or("?")
            ),
        });
    }
    None
}

/// Rule 2: previews — created in-window from pack memory, bypassing the cache.
fn rule_preview(n: &str) -> Option<Verdict> {
    PREVIEW_MARKERS.iter().any(|p| p.hit(n)).then(|| Verdict {
        class: Class::Early(Early::Tp),
        reason: "preview bypass — created in the tp window straight from pack memory \
                 (preview_texture_pack_loader 0x1403b0ef0), NOT via the CTexture cache: \
                 a late cache overwrite can not win; needs tp injection"
            .into(),
    })
}

/// Rule 3: `tp` system/UI/terrain/sky texture + `.dat` names.
fn rule_tp_names(n: &str) -> Option<Verdict> {
    TP_NAMES.iter().any(|p| p.hit(n)).then(|| Verdict {
        class: Class::Early(Early::Tp),
        reason: "tp window (pack_loader_tp 0x1403f0d20) reads it via pack_master_map_query \
                 + d3dx from pack memory — must be resident before tp"
            .into(),
    })
}

/// Rule 4: shaders — `csdp` compiles pre-sequence (stage 1); `sdp` in its own
/// window.
fn rule_shaders(key: Option<&str>, n: &str) -> Option<Verdict> {
    if key == Some("csdp") {
        return Some(Verdict {
            class: Class::Pre,
            reason: "csdp shader bytecode: compiled to COM shaders at load \
                     (boot_pack_system_driver, stage 1 — before the asset sequence)"
                .into(),
        });
    }
    if key == Some("sdp") || SHADER_EXTS.iter().any(|p| p.hit(n)) {
        return Some(Verdict {
            class: Class::Early(Early::Sdp),
            reason: "shader bytecode: compiled to COM shaders in the sdp window \
                     (pack_loader_sdp 0x1403f0850)"
                .into(),
        });
    }
    None
}

/// Rule 5: models / meshes / prop-dir data (parsed + built in-window).
fn rule_models(pack: Option<&str>, n: &str, famb: bool) -> Option<Verdict> {
    let in_model_dir = MODEL_DIRS.iter().any(|d| n.starts_with(&format!("{d}/")));
    if !(MODEL_EXTS.iter().any(|p| p.hit(n)) || in_model_dir) {
        return None;
    }
    // a .dir inside a family-B image pack is that pack's index data, not a
    // texture and not an mcp prop set
    if famb && n.ends_with(".dir") {
        return Some(Verdict {
            class: Class::Unknown,
            reason: format!(
                "pack {} index data (.dir), not a texture — not late-viable",
                pack.unwrap_or("?")
            ),
        });
    }
    Some(Verdict {
        class: Class::Early(Early::Models),
        reason: "model/mesh or prop-dir data: parsed + built in-window \
                 (mcp 0x1403ee710 / ep 0x1403ef9f0 / cp 0x1403ef100)"
            .into(),
    })
}

/// Rule 6: CSV tables (csvp reservoir filled in stage 1, before tp).
fn rule_csv(key: Option<&str>, n: &str) -> Option<Verdict> {
    if key == Some("csvp") || n.ends_with(".csv") {
        return Some(Verdict {
            class: Class::Pre,
            reason: "csv table: resident in the csvp reservoir (pack_loader_csvp 0x1403ef5b0, \
                     boot_pack_system_driver stage 1) — never late; merge/serve before the csvp load"
                .into(),
        });
    }
    None
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub class: Class,
    pub reason: String,
}

/// Classify every data effect of one manifest, worst class first (so a mod
/// author sees what will NOT win before what will).
pub fn classify_manifest(m: &Manifest) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    rows.extend(pack_rows(m));
    rows.extend(csv_rows(m));
    rows.extend(asset_rows(m));
    rows.extend(file_rows(m));
    rows.sort_by_key(|r| r.risk());
    rows
}

/// `[[pack.entry]]` rows.
fn pack_rows(m: &Manifest) -> Vec<Row> {
    let mut rows = Vec::new();
    for p in &m.pack {
        for e in &p.entry {
            let v = classify_entry(Some(&p.pack), &e.name);
            rows.push(Row {
                mod_id: m.id.clone(),
                mechanism: format!("pack:{}", p.pack),
                target: e.name.clone(),
                class: v.class,
                reason: v.reason,
            });
        }
    }
    rows
}

/// `[[csv]]` rows (always classified in the csvp context).
fn csv_rows(m: &Manifest) -> Vec<Row> {
    m.csv
        .iter()
        .map(|c| {
            let v = classify_entry(Some("csvp"), &c.name);
            Row {
                mod_id: m.id.clone(),
                mechanism: "csvp".into(),
                target: c.name.clone(),
                class: v.class,
                reason: v.reason,
            }
        })
        .collect()
}

/// `[[asset]]` rows, window-time by their host pack.
fn asset_rows(m: &Manifest) -> Vec<Row> {
    m.asset.iter().map(|a| asset_row(m, a)).collect()
}

/// One `[[asset]]` row. The family-A host packs load in boot order
/// (tp → ... → mcp/ep/cp → vp); a merge-hosted payload is resident only while
/// that pack is the current one. `host = "auto"` is resolved at load time from
/// the per-build dir index — the window depends on the resolved pack, so
/// classify labels it as the family-A bucket and says so.
fn asset_row(m: &Manifest, a: &AssetSection) -> Row {
    if a.host.eq_ignore_ascii_case(a9_mod::manifest::HOST_AUTO) {
        return Row {
            mod_id: m.id.clone(),
            mechanism: "asset:auto".into(),
            target: a.name.clone(),
            class: Class::Early(Early::Models),
            reason: "[[asset]] host = \"auto\": the host pack is resolved at load time \
                     from the embedded per-build dir index (dir -> pack); the residency \
                     window is that of the resolved family-A pack — check `a9ml list` for \
                     the concrete host"
                .into(),
        };
    }
    let (early, hostname) = match pack::classify_key(&a.host).as_str() {
        "tp" => (Early::Tp, "tp"),
        "vp" => (Early::Vp, "vp"),
        _ => (Early::Models, "mcp/ep/cp"),
    };
    Row {
        mod_id: m.id.clone(),
        mechanism: format!("asset:{}", a.host),
        target: a.name.clone(),
        class: Class::Early(early),
        reason: format!(
            "[[asset]] merge-served: appended to the {hostname} pack's decoded stream \
             + an i.dat row (off from the shared layout) — window-time only (the payload \
             is resident only while {hostname} is the current pack){}",
            if a.override_vanilla {
                "; replaces a shipped i.dat row (override_vanilla)"
            } else {
                ""
            }
        ),
    }
}

/// `[[file]]` rows — classified by the destination's basename, no pack context.
fn file_rows(m: &Manifest) -> Vec<Row> {
    m.file
        .iter()
        .map(|f| {
            let base = f.dest.rsplit(['/', '\\']).next().unwrap_or(&f.dest);
            let v = classify_entry(None, base);
            Row {
                mod_id: m.id.clone(),
                mechanism: "file".into(),
                target: f.dest.clone(),
                class: v.class,
                reason: format!("[[file]] new loose copy — {}", v.reason),
            }
        })
        .collect()
}

// ------------------------------------------------------------ CLI command

/// `a9ml classify [<mod-dir-or-mods-dir>] [--entry NAME [--pack P]]`.
pub fn run(path: Option<&Path>, entry: Option<&str>, pack: Option<&str>) -> Result<()> {
    if let Some(name) = entry {
        let v = classify_entry(pack, name);
        println!("entry {name:?} (pack {})", pack.unwrap_or("-"));
        println!("  class:  {}", v.class.label());
        println!("  reason: {}", v.reason);
        return Ok(());
    }
    let Some(path) = path else {
        bail!("give a mod dir / mods dir, or --entry NAME [--pack PACK]");
    };

    let mods = load_mods(path)?;
    if mods.is_empty() {
        bail!("no mods found at {}", path.display());
    }

    print_header();

    let mut overall = Summary::default();
    for (label, m) in &mods {
        print_mod(m, label, &mut overall);
    }
    println!("== overall: {} mod(s), {}", mods.len(), overall.compact());
    Ok(())
}

/// The command's banner: the model provenance and how to read the classes.
fn print_header() {
    println!("== a9ml classify — where the game consumes each data effect");
    println!("   model: notes/24 round 7 — stage 1 (i.dat/csdp/csvp) then");
    println!("          tp → sdp → me0-3 → mtentr0/mtcutp0 → mcp → ep → cp → vp");
    println!("   early:* = consumed inside that boot window — serve with pack_serving=inject or merge");
    println!("             pre-shader/pre-csv = consumed in stage 1 · unknown = be conservative");
    println!();
}

/// One mod's block: its rows, its summary, its verdict. Accumulates into
/// `overall`.
fn print_mod(m: &Manifest, label: &str, overall: &mut Summary) {
    let rows = classify_manifest(m);
    let s = Summary::of(&rows);
    println!("== mod '{}' ({label})", m.id);
    for r in &rows {
        println!("   {}", r.line());
    }
    println!("   summary: {}", s.compact());
    if rows.is_empty() {
        if m.loose_asset.is_empty() {
            println!("   verdict: no [[pack]]/[[csv]]/[[asset]]/[[file]] data effects");
        } else {
            println!(
                "   verdict: no window-model data effects; {} [[loose_asset]] path \
                 redirect(s) served live at the game's own open (no residency window)",
                m.loose_asset.len()
            );
        }
    } else if s.all_known() {
        println!(
            "   verdict: all {} target(s) have known consumption points — see the classes",
            rows.len()
        );
    } else {
        println!(
            "   verdict: {} target(s) unknown — treat conservatively (window residency)",
            s.unknown
        );
    }
    println!();
    // accumulate
    overall.early += s.early;
    overall.pre += s.pre;
    overall.unknown += s.unknown;
    for w in s.windows {
        if !overall.windows.contains(&w) {
            overall.windows.push(w);
        }
    }
}

/// Accept either a single mod dir (holding `mod.toml`) or a mods dir.
fn load_mods(path: &Path) -> Result<Vec<(String, Manifest)>> {
    if path.join("mod.toml").is_file() {
        let text = fs::read_to_string(path.join("mod.toml"))
            .with_context(|| format!("reading {}", path.join("mod.toml").display()))?;
        let mut m: Manifest = toml::from_str(&text).context("parsing mod.toml")?;
        m.validate().map_err(|e| anyhow::anyhow!("{e}"))?;
        a9_mod::scan::expand_asset_folders(&mut m, path).map_err(|e| anyhow::anyhow!("{e}"))?;
        m.validate().map_err(|e| anyhow::anyhow!("{e}"))?;
        return Ok(vec![(path.display().to_string(), m)]);
    }
    if !path.is_dir() {
        bail!("not a directory: {}", path.display());
    }
    let mut out = Vec::new();
    for s in a9_mod::scan::scan(path) {
        match s.manifest {
            Ok(m) => out.push((s.dir.display().to_string(), m)),
            Err(e) => println!("   broken mod {}: {e}", s.dir.display()),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn class(pack: Option<&str>, name: &str) -> Class {
        classify_entry(pack, name).class
    }

    #[test]
    fn famb_livery_texture_is_window_time() {
        // a me3 `_d_*.dds` — served during the family-B window into the CTexture cache
        assert_eq!(class(Some("me3"), "_d_733_1000.dds"), Class::Early(Early::Famb));
        assert_eq!(class(Some("me3.bin"), "_d_733_1000.dds"), Class::Early(Early::Famb));
        // every suffix family + the other family-B image packs
        for (p, n) in [
            ("me0", "_m_museum4_a.dds"),
            ("me1", "_l_museum4_a.dds"),
            ("me2", "_n_body.dds"),
            ("me3", "_a_b_roof.dds"),
            ("mtentr0", "_d_113dd.dds"),
            ("mtcutp0", "_d_df200.dds"),
        ] {
            assert_eq!(class(Some(p), n), Class::Early(Early::Famb), "{p}:{n}");
        }
        // …but NOT in a non-family-B-image pack (tp is a family-A stream)
        assert_ne!(class(Some("tp"), "_d_733_1000.dds"), Class::Early(Early::Famb));
        // an uncontrollable pack name can't be classified as a window family
        assert_eq!(class(Some("auto"), "_d_733_1000.dds"), Class::Unknown);
    }

    #[test]
    fn famb_wins_over_preview_suffix_but_tp_preview_is_early() {
        // a livery texture in a family-B image pack ending `_f.dds` still rides
        // the me/mt window (the preview loader only classifies the tp pack's
        // previewtexture.dir)
        for n in ["_d_skt044_f.dds", "_l_b064_f.dds", "_m_skt042_f.dds"] {
            assert_eq!(class(Some("me3"), n), Class::Early(Early::Famb), "{n}");
        }
        // …whereas a real tp preview name is early:tp
        let v = classify_entry(Some("tp"), "form_113_1_f.dds");
        assert_eq!(v.class, Class::Early(Early::Tp), "{}", v.reason);
        assert!(v.reason.contains("preview bypass"), "{}", v.reason);
    }

    #[test]
    fn preview_is_early_tp_bypass() {
        let v = classify_entry(Some("tp"), "previewtexture.dir");
        assert_eq!(v.class, Class::Early(Early::Tp));
        assert!(v.reason.contains("preview bypass"), "{}", v.reason);
        for n in ["form_113_1.dds", "form_x_s.dds", "foo_f.dds", "foo_w.dds", "foo_s.dds"] {
            let v = classify_entry(Some("tp"), n);
            assert_eq!(v.class, Class::Early(Early::Tp), "{n}");
            assert!(v.reason.contains("preview bypass"), "{n}: {}", v.reason);
        }
    }

    #[test]
    fn tp_system_textures_are_early_tp() {
        for n in [
            "env_map.dds",
            "noise.dds",
            "window_color_table.tga",
            "window_timeline_table.tga",
            "tonemap_table.bmp",
            "map_wall.tga",
            "map_wall_bump.tga",
            "wavesbump.tga",
            "heightmap.tga",
            "heightmap_bump.tga",
            "edit.dds",
            "height_map_alpha_table.tga",
            "height_map_paint_noise.dat",
            "firework3.tga",
            "keikoku.tga",
            "snow.tga",
            "steam.tga",
            "volcano.dat",
            "cloud_new.tga",
            "water.dds",
            "sun.bmp",
            "sunflare.dds",
            "lensflar2.tga",
            "moon01.tga",
        ] {
            assert_eq!(class(Some("tp"), n), Class::Early(Early::Tp), "{n}");
        }
    }

    #[test]
    fn models_are_early() {
        for (p, n) in [
            ("mcp", "clock_long.x"),
            ("mcp", "arrow/arrow.x"),
            ("cp", "structure/harbors/_y_area_harbor.x"),
            ("ep", "ship/_c_fu_00.x"),
            ("mcp", "_s_room_03.x"),
            ("mcp", "light/light.dir"),
        ] {
            assert_eq!(class(Some(p), n), Class::Early(Early::Models), "{p}:{n}");
        }
    }

    #[test]
    fn shaders() {
        // sdp (and a bare .vs/.ps) → the sdp window
        assert_eq!(class(Some("sdp"), "color.vs"), Class::Early(Early::Sdp));
        assert_eq!(class(None, "color.ps"), Class::Early(Early::Sdp));
        // csdp → stage 1, pre-sequence
        assert_eq!(class(Some("csdp"), "color.vs"), Class::Pre);
        assert_eq!(class(Some("csdp"), "color.ps"), Class::Pre);
    }

    #[test]
    fn csv_tables_are_pre() {
        assert_eq!(class(Some("csvp"), "train_comment.csv"), Class::Pre);
        assert_eq!(class(None, "train_form.csv"), Class::Pre);
    }

    #[test]
    fn famb_index_dir_is_unknown() {
        // a .dir in a family-B image pack is index data, not a texture
        let v = classify_entry(Some("me3"), "previewtexture2.dir");
        // (previewtexture* wins first — use a plain name for the .dir rule)
        assert!(v.reason.contains("preview"), "{}", v.reason);
        assert_eq!(class(Some("me3"), "whatever.dir"), Class::Unknown);
    }

    #[test]
    fn unknown_is_conservative() {
        let v = classify_entry(Some("tp"), "mystery_blob.dat");
        assert_eq!(v.class, Class::Unknown);
        assert!(v.reason.contains("consumption point not in the known set"), "{}", v.reason);
        assert_eq!(class(None, "random.bin"), Class::Unknown);
    }

    #[test]
    fn summary_rolls_up_and_orders_windows() {
        let rows = vec![
            Row { mod_id: "m".into(), mechanism: "pack:me3".into(), target: "_d_x.dds".into(), class: Class::Early(Early::Famb), reason: String::new() },
            Row { mod_id: "m".into(), mechanism: "pack:tp".into(), target: "env_map.dds".into(), class: Class::Early(Early::Tp), reason: String::new() },
            Row { mod_id: "m".into(), mechanism: "pack:mcp".into(), target: "clock_long.x".into(), class: Class::Early(Early::Models), reason: String::new() },
            Row { mod_id: "m".into(), mechanism: "pack:sdp".into(), target: "c.vs".into(), class: Class::Early(Early::Sdp), reason: String::new() },
            Row { mod_id: "m".into(), mechanism: "asset:vp".into(), target: "_t_new_a.x".into(), class: Class::Early(Early::Vp), reason: String::new() },
            Row { mod_id: "m".into(), mechanism: "csvp".into(), target: "t.csv".into(), class: Class::Pre, reason: String::new() },
            Row { mod_id: "m".into(), mechanism: "file".into(), target: "data/x.bin".into(), class: Class::Unknown, reason: String::new() },
        ];
        let s = Summary::of(&rows);
        assert_eq!(s.early, 5);
        assert_eq!(s.pre, 1);
        assert_eq!(s.unknown, 1);
        // boot order, deduped
        assert_eq!(s.windows, vec!["tp", "sdp", "me/mt", "mcp/ep/cp", "vp"]);
        assert_eq!(
            s.compact(),
            "early: 5 (windows: tp,sdp,me/mt,mcp/ep/cp,vp), pre: 1, unknown: 1"
        );
        assert!(!s.all_known());
    }

    #[test]
    fn manifest_rows_cover_all_three_sections() {
        let m: Manifest = toml::from_str(
            "id=\"demo\"\nname=\"D\"\nversion=\"1.0.0\"\nauthor=\"t\"\n\
             [[pack]]\npack=\"me3\"\n\
             [[pack.entry]]\nname=\"_d_a.dds\"\nwidth=1\nheight=1\nfile=\"e/a.dds\"\n\
             [[csv]]\nname=\"train_comment.csv\"\nfile=\"csv/t.csv\"\n\
             [[file]]\ndest=\"data/texture/newthing.dds\"\nfile=\"files/n.dds\"\n",
        )
        .unwrap();
        m.validate().unwrap();
        let rows = classify_manifest(&m);
        assert_eq!(rows.len(), 3);
        // worst-first: unknown ([[file]]), then early (pack), then pre (csv)
        assert_eq!(rows[0].class, Class::Unknown);
        assert_eq!(rows[0].mechanism, "file");
        assert_eq!(rows[1].class, Class::Early(Early::Famb));
        assert_eq!(rows[1].mechanism, "pack:me3");
        assert_eq!(rows[2].class, Class::Pre);
        assert_eq!(rows[2].mechanism, "csvp");
        assert!(rows.iter().all(|r| r.mod_id == "demo"));
    }

    #[test]
    fn asset_rows_are_window_time_by_host() {
        let m: Manifest = toml::from_str(
            "id=\"na\"\nname=\"N\"\nversion=\"1.0.0\"\nauthor=\"t\"\n\
             [[asset]]\nname=\"_t_new_a.x\"\ndir=\"train\\\\new\"\nhost=\"vp\"\nfile=\"a.x\"\n\
             [[asset]]\nname=\"_t_p_a.x\"\ndir=\"train\\\\p\"\nhost=\"tp\"\nfile=\"b.x\"\n\
             [[asset]]\nname=\"_t_m_a.x\"\ndir=\"train\\\\m\"\nhost=\"mcp\"\nfile=\"c.x\"\n",
        )
        .unwrap();
        m.validate().unwrap();
        let rows = classify_manifest(&m);
        assert_eq!(rows.len(), 3);
        let by = |t: &str| rows.iter().find(|r| r.target == t).unwrap();
        assert_eq!(by("_t_new_a.x").class, Class::Early(Early::Vp));
        assert_eq!(by("_t_new_a.x").mechanism, "asset:vp");
        assert!(by("_t_new_a.x").reason.contains("merge-served"), "{}", by("_t_new_a.x").reason);
        assert!(by("_t_new_a.x").reason.contains("window-time only"), "{}", by("_t_new_a.x").reason);
        assert_eq!(by("_t_p_a.x").class, Class::Early(Early::Tp));
        assert_eq!(by("_t_m_a.x").class, Class::Early(Early::Models));
        // windows in boot order: tp, then mcp/ep/cp, then vp
        let s = Summary::of(&rows);
        assert_eq!(s.windows, vec!["tp", "mcp/ep/cp", "vp"]);
        // the override_vanilla note is carried in the reason
        let ov: Manifest = toml::from_str(
            "id=\"na\"\nname=\"N\"\nversion=\"1.0.0\"\nauthor=\"t\"\n\
             [[asset]]\nname=\"_t_van_a.x\"\ndir=\"train\\\\v\"\nhost=\"vp\"\nfile=\"a.x\"\noverride_vanilla=true\n",
        )
        .unwrap();
        ov.validate().unwrap();
        let r = classify_manifest(&ov);
        assert!(r[0].reason.contains("override_vanilla"), "{}", r[0].reason);
    }

    #[test]
    fn auto_host_asset_is_labelled_asset_auto() {
        let m: Manifest = toml::from_str(
            "id=\"auto\"\nname=\"A\"\nversion=\"1.0.0\"\nauthor=\"t\"\n\
             [[asset]]\nfolder=\"assets\"\n",
        )
        .unwrap();
        m.validate().unwrap();
        // (folder expansion is the scan's job; classify sees the raw section,
        // so a folder-form row has an empty name — that is fine here)
        let rows = classify_manifest(&m);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].mechanism, "asset:auto");
        assert_eq!(rows[0].class, Class::Early(Early::Models));
        assert!(rows[0].reason.contains("resolved at load time"), "{}", rows[0].reason);
    }
}
