//! The M3 runtime overlay — the `CFile::Open` redirect (the serving variant
//! the user picked 2026-10-16: one hook + a loader-managed, content-hashed
//! cache under `<mods_dir>/cache/`; the game's own open/read/close semantics
//! stay genuine Win32 downstream of the redirect).
//!
//! Flow: at boot the engine collects the enabled mods' `[[pack]]`/`[[csv]]`
//! effects (priority order, overlap losers already refused by the resolver),
//! installs ONE hook on `cfile_open` (a trampoline — the original stays
//! callable), and pre-reads nothing else. At the game's own open of
//! `data\<pack>.bin`, `my_cfile_open` normalizes the path, and on a hit:
//! builds the merged pack once per run (decode original → apply entry ops →
//! re-encode via a9-formats, budget-checked), writes the cache atomically,
//! and calls the ORIGINAL open with the cache path. Anything unmatched, any
//! failure, any budget overflow → the original path, untouched behavior,
//! loud log line. Never panics, never unwinds: this runs in the game's own
//! call path.
//!
//! M5 W1 adds two keys to the same hook: a family-A host pack (its decoded
//! stream gets the mods' `[[asset]]` payloads appended) and `data\i.dat`
//! (the master index gets the matching rows). The index build runs AFTER
//! every referenced host pack's cache (atomicity: a host failure refuses the
//! index too, so no row can point past a vanilla image) and takes its row
//! offsets from the same [`a9_formats::overlay::plan_family_a`] the host
//! merges used. `i.dat` is served from `<mods_dir>\cache\i.dat`.
//!
//! M6 adds `[[loose_asset]]`: game files the game opens BY PATH (not content
//! inside a pack). Each payload is materialized into `<mods_dir>/cache/loose/`
//! at install time (a REAL file — `CFile::Read`/`GetLength` are plain Win32 on
//! the handle, so there is no memory-source branch to exploit) and every
//! `CFile::Open` is tested against the compiled patterns BEFORE the pack
//! logic; on a match the open path is rewritten to the cache file. The table
//! is scanned only when it is non-empty.
//!
//! Cache staleness key: FNV-1a over (pack, per-op mod id + version + entry
//! name + payload file mtime + size) — a changed mod set or a touched
//! payload file rebuilds the cache; an unchanged set boots from cache with
//! zero merge cost. The key lives beside the cache as `<pack>.key`.

use crate::{log, patch, sys};
use a9_formats::overlay::ImageOp;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// merged-container decoded-size ceiling (the engine decode workspace is
/// 200 MB; the mod model's asserted budget is ~190 MB)
const BUDGET: usize = 190 * 1024 * 1024;

/// One entry op with its source location (payload read from the mod dir).
struct PendingImageOp {
    mod_id: String,
    mod_version: String,
    name: String,
    width: u32,
    height: u32,
    mod_dir: PathBuf,
    file: PathBuf,
}

/// One csvp table op, resolved by the engine (payload in hand, row overlap
/// refusal already applied).
struct PendingCsvOp {
    mod_id: String,
    mod_version: String,
    kind: a9_formats::overlay::CsvOpKind,
    name: String,
    payload: Vec<u8>,
    /// first-column keys of a `mode = "rows"` patch (fingerprint only)
    row_keys: Vec<String>,
}

/// One `[[asset]]` op, resolved by the engine (payload read at merge time).
struct PendingAssetOp {
    mod_id: String,
    mod_version: String,
    /// host family-A pack key (vp/tp/mcp/ep/cp)
    host: String,
    /// bare i.dat / master-map key
    name: String,
    /// i.dat dir field
    dir: String,
    mod_dir: PathBuf,
    /// payload file, relative to the mod dir
    file: PathBuf,
    /// allow replacing a row that exists in the SHIPPED i.dat
    override_vanilla: bool,
}

/// One auto-merged `.dir` listing op (M5-U) — payload already rebuilt by the
/// engine (`listing_extend`), appended to its host after the host's asset
/// payloads.
struct PendingListingOp {
    host: String,
    name: String,
    dir: String,
    payload: Vec<u8>,
}

#[derive(Default)]
struct Packs {
    /// family-B image packs (me*/mt*): pack name → ops (priority order)
    images: BTreeMap<String, Vec<PendingImageOp>>,
    /// the 0x204 csv container (csvp): table ops (priority order)
    csvs: Vec<PendingCsvOp>,
    /// [[asset]] ops (M5 W1), in resolved order (priority, then manifest order)
    assets: Vec<PendingAssetOp>,
    /// auto-merged `.dir` listings (M5-U), in first-appearance group order
    listings: Vec<PendingListingOp>,
    /// packs (and "i.dat") the M5 tailgate serves: their payloads/rows are
    /// appended into the workspace by the spliced decode funnel, so the
    /// overlay must NEITHER redirect their `CFile::Open` NOR build a cache
    tailgated: std::collections::BTreeSet<String>,
    /// per-host vanilla family-A declared size (the append base the tailgate
    /// stub uses, and the `i.dat` row offset) — read once at boot for every
    /// host referenced by an `[[asset]]`
    declared: BTreeMap<String, usize>,
    /// the materialized `[[loose_asset]]` table (M6): compiled patterns + their
    /// cache copies, in resolved order. Empty = the hook skips the scan.
    loose: Vec<LooseEntry>,
}

/// One materialized `[[loose_asset]]` redirect: a compiled pattern and the
/// loader-cache file whose bytes must be served in its place.
struct LooseEntry {
    mod_id: String,
    pattern: a9_mod::loose::Pattern,
    /// the cache file, relative to the game CWD (`mods\cache\loose\<name>`)
    cache_rel: PathBuf,
}

struct Inner {
    packs: Packs,
    mods_dir: PathBuf,
    /// the mods dir as configured (relative, e.g. "mods") — cache serve
    /// paths are built from it, relative to the game CWD like the game's
    /// own `data\x.bin` opens
    mods_dir_rel: PathBuf,
    exe_dir: PathBuf,
    original: patch::Trampoline,
    logger: log::Logger,
    /// debug trace of every intercepted open (a9loader.toml log_level=debug)
    trace_opens: bool,
    /// resolved serve path (or remembered error) per pack — built at most
    /// once per run per pack; a failed build fails loudly ONCE and then
    /// serves the original
    built: std::sync::Mutex<BTreeMap<String, Result<PathBuf, String>>>,
}

static G: std::sync::OnceLock<Inner> = std::sync::OnceLock::new();

/// One csvp table op the engine hands to the overlay — payload already read
/// (and, for row patches, already filtered by per-row overlap refusal).
pub struct CsvOpPlan {
    pub mod_id: String,
    pub mod_version: String,
    pub kind: a9_formats::overlay::CsvOpKind,
    pub name: String,
    pub payload: Vec<u8>,
    pub row_keys: Vec<String>,
}

/// One `[[asset]]` op the engine hands to the overlay.
pub struct AssetOpPlan {
    pub mod_id: String,
    pub mod_version: String,
    /// host family-A pack key (vp/tp/mcp/ep/cp)
    pub host: String,
    /// bare i.dat / master-map key
    pub name: String,
    /// i.dat dir field
    pub dir: String,
    /// the owning mod directory (payloads are mod-relative)
    pub mod_dir: PathBuf,
    /// payload file, relative to the mod dir
    pub file: String,
    /// allow replacing a row that exists in the SHIPPED i.dat
    pub override_vanilla: bool,
}

/// One auto-merged `.dir` group listing (M5-U) the engine derived from the
/// mods' own `[[asset]]` rows.
///
/// `payload` is the ALREADY-REBUILT listing (`a9_formats::overlay::
/// listing_extend`): the shipped listing bytes verbatim + the mods' new names,
/// deduped, NUL-terminated UTF-16LE. `name` is the listing row key
/// (`<group>.dir`); `dir` is the shipped row's own `dir` field, so the
/// tailgate's appended duplicate reproduces it exactly.
pub struct ListingOpPlan {
    pub host: String,
    pub name: String,
    pub dir: String,
    pub payload: Vec<u8>,
}

/// One `[[loose_asset]]` op the engine hands to the overlay (M6): a game path
/// pattern + the mod-relative payload that must be served in its place.
///
/// The overlay materializes the payload into `<mods_dir>/cache/loose/` at
/// install time and tests every `CFile::Open` against the compiled pattern.
pub struct LooseOpPlan {
    pub mod_id: String,
    /// the manifest glob, as authored (normalized at compile time)
    pub pattern: String,
    /// the owning mod directory (the payload is mod-relative)
    pub mod_dir: PathBuf,
    /// payload file, relative to the mod dir
    pub file: String,
}

/// The plan the engine hands over at boot. Consumed by [`install`].
pub struct Plan {
    /// `data\<pack>.bin` name (e.g. "mtentr0") → entry ops in priority order,
    /// each with its owning mod dir (payloads are mod-relative)
    pub images: BTreeMap<String, Vec<(PathBuf, String, String, a9_mod::manifest::PackEntry)>>,
    /// csvp table ops in priority order
    pub csvs: Vec<CsvOpPlan>,
    /// `[[asset]]` ops in resolved order (priority, then manifest order); the
    /// payloads ride their host pack's decoded stream and their i.dat rows are
    /// written at the `data\i.dat` open (M5 W1)
    pub assets: Vec<AssetOpPlan>,
    /// auto-merged `.dir` group listings (M5-U), derived from the `[[asset]]`
    /// rows above: each one appends to its host AFTER the host's asset payloads
    /// and rewrites its shipped i.dat listing row (merge: in place; tailgate:
    /// an appended duplicate — later-wins, memory #316)
    pub listings: Vec<ListingOpPlan>,
    /// packs (and "i.dat") the M5 tailgate serves — the overlay must not
    /// redirect or cache them (`asset_serving = "tailgate"`; empty in merge
    /// mode and for any host the tailgate could not take)
    pub tailgated: std::collections::BTreeSet<String>,
    /// vanilla family-A declared size per host pack (and "i.dat"), read once
    /// at boot — the tailgate append base == the i.dat row offset; a merged
    /// host ignores it and uses its decoded length (equal for vanilla packs)
    pub declared: BTreeMap<String, usize>,
    /// `[[loose_asset]]` ops in resolved order (M6): game files opened BY
    /// PATH, matched by a case-insensitive glob, served from a loader-managed
    /// cache copy. Empty in the common case — the hook then skips the scan.
    pub loose: Vec<LooseOpPlan>,
}

/// Install the overlay (boot time): stash the plan, hook `cfile_open`.
/// `entry_rva`/`expect` come from the address book for the running build;
/// `mods_dir` is the configured dir name (e.g. "mods").
pub unsafe fn install(
    plan: Plan,
    mods_dir: PathBuf,
    exe_dir: PathBuf,
    entry_rva: usize,
    expect: &[u8],
    logger: log::Logger,
    trace_opens: bool,
) -> Result<(), String> {
    unsafe {
        let base = sys::GetModuleHandleW(std::ptr::null()) as *mut u8;
        let entry = base.add(entry_rva);
        let tramp = patch::install_trampoline(
            entry,
            my_cfile_open as *const core::ffi::c_void,
            expect,
            sys::PAGE_EXECUTE_READWRITE,
        )?;
        let mods_dir_abs = exe_dir.join(&mods_dir);
        // M6: materialize the [[loose_asset]] cache copies now (fail-open per
        // asset) and compile their patterns — the hook scans this table.
        let loose = materialize_loose(&plan.loose, &mods_dir_abs, &mods_dir, &logger);
        let packs = Packs {
            images: plan
                .images
                .into_iter()
                .map(|(pack, ops)| {
                    (
                        pack,
                        ops.into_iter()
                            .map(|(dir, id, ver, e)| PendingImageOp {
                                mod_id: id,
                                mod_version: ver,
                                name: e.name,
                                width: e.width,
                                height: e.height,
                                mod_dir: dir,
                                file: PathBuf::from(&e.file),
                            })
                            .collect(),
                    )
                })
                .collect(),
            csvs: plan
                .csvs
                .into_iter()
                .map(|c| PendingCsvOp {
                    mod_id: c.mod_id,
                    mod_version: c.mod_version,
                    kind: c.kind,
                    name: c.name,
                    payload: c.payload,
                    row_keys: c.row_keys,
                })
                .collect(),
            assets: plan
                .assets
                .into_iter()
                .map(|a| PendingAssetOp {
                    mod_id: a.mod_id,
                    mod_version: a.mod_version,
                    host: a.host,
                    name: a.name,
                    dir: a.dir,
                    mod_dir: a.mod_dir,
                    file: PathBuf::from(&a.file),
                    override_vanilla: a.override_vanilla,
                })
                .collect(),
            listings: plan
                .listings
                .into_iter()
                .map(|l| PendingListingOp {
                    host: l.host,
                    name: l.name,
                    dir: l.dir,
                    payload: l.payload,
                })
                .collect(),
            tailgated: plan.tailgated,
            declared: plan.declared,
            loose,
        };
        let _ = G.set(Inner {
            packs,
            mods_dir: mods_dir_abs,
            mods_dir_rel: mods_dir,
            exe_dir,
            original: tramp,
            logger,
            trace_opens,
            built: std::sync::Mutex::new(BTreeMap::new()),
        });
        Ok(())
    }
}

type CFileOpenFn =
    unsafe extern "system" fn(*mut core::ffi::c_void, *const u16, u32, *mut core::ffi::c_void) -> i32;

/// The hook — MFC `CFile::Open(this, path, flags, pError) -> BOOL`.
/// Total: any failure path serves the original open unchanged.
unsafe extern "system" fn my_cfile_open(
    this: *mut core::ffi::c_void,
    path: *const u16,
    flags: u32,
    perr: *mut core::ffi::c_void,
) -> i32 {
    unsafe {
        let original: CFileOpenFn = match G.get() {
            Some(inner) => core::mem::transmute(inner.original.as_ptr()),
            None => return 1, // not installed — cannot happen (we ARE the hook)
        };
        let inner = G.get().unwrap();
        // normalize the path: lowercase ASCII, '\' slashes, no leading ".\"
        let norm = match path_to_string(path) {
            Some(s) => s,
            None => return original(this, path, flags, perr), // unreadable: pass through
        };
        if inner.trace_opens {
            inner.logger.debug(&format!("open trace: {norm}"));
        }
        // M6 [[loose_asset]]: a game file opened DIRECTLY by path (not pack
        // content) — serve the loader's cache copy in its place. The scan only
        // runs when the table is non-empty; `norm` is already normalized
        // (lowercase + backslashes), which is exactly what the pattern wants.
        if !inner.packs.loose.is_empty() {
            if let Some(entry) = inner
                .packs
                .loose
                .iter()
                .find(|l| l.pattern.matches_norm(&norm))
            {
                if inner.trace_opens {
                    inner.logger.debug(&format!(
                        "loose match: {norm} -> {} (mod {})",
                        entry.cache_rel.display(),
                        entry.mod_id
                    ));
                }
                let wide: Vec<u16> = entry
                    .cache_rel
                    .to_string_lossy()
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                return original(this, wide.as_ptr(), flags, perr);
            }
        }
        // M4 inject capture: `data\<pack>.bin` opens feed the injector's
        // pack-identity slot (a no-op unless the injector is live and the
        // pack carries inject ops; merge mode never installs it). Loaders
        // are sequential single-threaded, so the capture is coherent at the
        // loader's runner call right before it fires (notes/24 R5).
        if let Some(pack) = pack_key(&norm) {
            crate::injector::note_pack_open(&pack);
        }
        let key = match pack_key(&norm).and_then(|p| overlay_key(inner, &p)) {
            Some(k) => k,
            None => return original(this, path, flags, perr), // not an overlayed pack: pass through
        };
        let cache = match ensure_cache(inner, &key) {
            Ok(p) => p,
            Err(_) => return original(this, path, flags, perr), // already logged
        };
        // the cache path is relative to the game CWD (short, like the game's
        // own "data\x.bin" relative opens)
        let wide: Vec<u16> = cache
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        original(this, wide.as_ptr(), flags, perr)
    }
}

/// UTF-16 path → normalized String (lowercase, backslashes, no leading ".\").
fn path_to_string(path: *const u16) -> Option<String> {
    if path.is_null() {
        return None;
    }
    let mut units = Vec::with_capacity(260);
    let mut p = path;
    unsafe {
        loop {
            let u = *p;
            if u == 0 {
                break;
            }
            units.push(u);
            if units.len() > 4096 {
                return None; // runaway string — refuse, pass through
            }
            p = p.add(1);
        }
    }
    let s = String::from_utf16_lossy(&units).to_ascii_lowercase().replace('/', "\\");
    let s = s.strip_prefix(".\\").unwrap_or(&s).to_string();
    Some(s)
}

/// `data\mtentr0.bin` (or a bare `mtentr0.bin`) → "mtentr0"; `data\i.dat` →
/// "i.dat"; `None` = not a pack path at all. Whether the pack is actually
/// OVERLAYED is decided by [`overlay_key`] — this only shapes the path.
fn pack_key(norm: &str) -> Option<String> {
    let (dir, file) = match norm.rfind('\\') {
        Some(i) => (&norm[..i], &norm[i + 1..]),
        None => ("", norm),
    };
    if !dir.is_empty() && dir != "data" {
        return None;
    }
    // i.dat is the family-A master index; it has no `.bin` suffix of its own
    if file == "i.dat" {
        return Some("i.dat".to_string());
    }
    let pack = file.strip_suffix(".bin")?;
    Some(pack.to_string())
}

/// The on-disk name for a cache/original key: packs are `<key>.bin`, the
/// master index is the game's own `i.dat` (no `.bin`).
fn pack_file_name(key: &str) -> String {
    if key == "i.dat" {
        "i.dat".to_string()
    } else {
        format!("{key}.bin")
    }
}

/// The pack named by the open, IF the overlay plan touches it — `None` =
/// passthrough untouched (no ops, no rebuild, no cache). Only packs with
/// mod effects are ever redirected; the live-run lesson (2026-10-16): a
/// broader match "merged 0 entries" on every pack the game opened, rebuilt
/// untouched 141 MB packs, and ran the family-B parser over family-A
/// packs (fail-open saved those, loudly).
fn overlay_key(inner: &Inner, pack: &str) -> Option<String> {
    // a pack the M5 tailgate serves is appended to in place at the decode —
    // it must NEVER be redirected to a cache (that is the whole point)
    if inner.packs.tailgated.contains(pack) {
        return None;
    }
    if pack == "csvp" {
        return (!inner.packs.csvs.is_empty()).then(|| pack.to_string());
    }
    if pack == "i.dat" {
        // the master index is redirected only when [[asset]] rows must ride it
        return (!inner.packs.assets.is_empty() || !inner.packs.listings.is_empty())
            .then(|| pack.to_string());
    }
    if inner.packs.images.contains_key(pack) {
        return Some(pack.to_string());
    }
    // a family-A host pack carrying appended [[asset]] payloads and/or an
    // auto-merged `.dir` listing
    (inner.packs.assets.iter().any(|a| a.host == pack)
        || inner.packs.listings.iter().any(|l| l.host == pack))
    .then(|| pack.to_string())
}

fn fnv(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// stat fingerprint of a payload file: (mtime ns, size) — 0,0 if unreadable
fn stat_key(p: &Path) -> (u64, u64) {
    match std::fs::metadata(p) {
        Ok(md) => {
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            (mtime, md.len())
        }
        Err(_) => (0, 0),
    }
}

/// Materialize the `[[loose_asset]]` cache copies (M6).
///
/// For each op: read the payload from the mod dir, write it to
/// `<mods_dir>/cache/loose/<name>` (overwritten every boot — it is a cache),
/// and compile its pattern into the table the hook scans. The name is the mod
/// id + a sanitized pattern so two mods can never collide.
///
/// FAIL-OPEN per asset, always: an unreadable payload, a failed write, or an
/// unwritable cache dir logs one loud line naming the mod + pattern and drops
/// that asset; the game's original open then proceeds untouched.
fn materialize_loose(
    ops: &[LooseOpPlan],
    mods_dir_abs: &Path,
    mods_dir_rel: &Path,
    logger: &log::Logger,
) -> Vec<LooseEntry> {
    if ops.is_empty() {
        return Vec::new();
    }
    let dir = mods_dir_abs.join("cache").join("loose");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        logger.err(&format!(
            "loose: cannot create {}: {e} — {} loose asset(s) dropped (fail-open)",
            dir.display(),
            ops.len()
        ));
        return Vec::new();
    }
    let mut out: Vec<LooseEntry> = Vec::with_capacity(ops.len());
    let mut used: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for op in ops {
        let payload = match std::fs::read(op.mod_dir.join(&op.file)) {
            Ok(b) => b,
            Err(e) => {
                logger.err(&format!(
                    "loose: mod {} pattern \"{}\": payload {} unreadable ({e}) — dropped \
                     (fail-open)",
                    op.mod_id, op.pattern, op.file
                ));
                continue;
            }
        };
        let name = unique_loose_name(&op.mod_id, &op.pattern, &mut used);
        let abs = dir.join(&name);
        if let Err(e) = std::fs::write(&abs, &payload) {
            logger.err(&format!(
                "loose: mod {} pattern \"{}\": writing {}: {e} — dropped (fail-open)",
                op.mod_id,
                op.pattern,
                abs.display()
            ));
            continue;
        }
        let cache_rel = mods_dir_rel.join("cache").join("loose").join(&name);
        let pattern = a9_mod::loose::Pattern::compile(&op.pattern);
        logger.line(&format!(
            "loose: mod {} pattern \"{}\" -> {} ({} B)",
            op.mod_id,
            pattern.text(),
            cache_rel.display(),
            payload.len()
        ));
        out.push(LooseEntry {
            mod_id: op.mod_id.clone(),
            pattern,
            cache_rel,
        });
    }
    out
}

/// The cache file name for one loose asset: `<mod-id>__<sanitized pattern>`
/// (two mods can never collide), disambiguated with a counter if two distinct
/// patterns in one mod sanitize to the same string.
fn unique_loose_name(
    mod_id: &str,
    pattern: &str,
    used: &mut std::collections::BTreeSet<String>,
) -> String {
    let base = format!(
        "{}__{}",
        sanitize_name(mod_id),
        sanitize_name(&a9_mod::loose::normalize(pattern))
    );
    let mut name = base.clone();
    let mut n = 2u32;
    while !used.insert(name.clone()) {
        name = format!("{base}__{n}");
        n += 1;
    }
    name
}

/// A cache-name-safe form of a string: keep `[a-z0-9._-]`, map the rest to
/// `_`, and bound the length (Windows path components are 255 chars).
fn sanitize_name(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    out.truncate(100);
    out
}

/// Build (or reuse) the cache for `key`; returns the cache path relative to
/// the game CWD ("mods\cache\<pack>.bin"). Failures are remembered and
/// repeat as loud-once errors.
///
/// The built-set lock is **not** held across the build: a merged's `i.dat`
/// build recurses into its host packs' caches (atomicity — hosts first), so a
/// re-entrant `ensure_cache` must not deadlock. The game's boot loaders are
/// sequential single-threaded, so a check/build/insert without holding the
/// lock across the build is safe; a redundant concurrent build would still
/// write the same content-hashed bytes.
fn ensure_cache(inner: &Inner, key: &str) -> Result<PathBuf, String> {
    {
        let built = inner
            .built
            .lock()
            .map_err(|_| "cache state poisoned".to_string())?;
        if let Some(r) = built.get(key) {
            return r.clone();
        }
    }
    let result = build_cache(inner, key);
    match &result {
        Ok((p, true)) => inner
            .logger
            .line(&format!("overlay {key}: cache fresh — reusing {}", p.display())),
        Ok((_, false)) => {} // build_cache logged the merge
        Err(e) => inner
            .logger
            .err(&format!("overlay {key}: {e} — serving the original this boot")),
    }
    let out = result.map(|(p, _)| p);
    let mut built = inner
        .built
        .lock()
        .map_err(|_| "cache state poisoned".to_string())?;
    built.insert(key.to_string(), out.clone());
    out
}

/// The merge: read the original pack, apply the ops, write the cache
/// atomically (tmp + rename) + the key file. Returns (serve path,
/// cache-was-fresh-reused).
///
/// `key == "i.dat"` is the M5 W1 master-index merge (see [`merge_idat`]):
/// host packs first (atomicity), then rows whose `off` comes from the same
/// `plan_family_a` the host merges used.
fn build_cache(inner: &Inner, key: &str) -> Result<(PathBuf, bool), String> {
    let cache_dir = inner.mods_dir.join("cache");
    std::fs::create_dir_all(&cache_dir).map_err(|e| format!("cache dir: {e}"))?;
    let name = pack_file_name(key);
    let original_path = inner.exe_dir.join("data").join(&name);
    let original = std::fs::read(&original_path)
        .map_err(|e| format!("reading the original {}: {e}", original_path.display()))?;

    let cache_path = cache_dir.join(&name);
    let key_path = cache_dir.join(format!("{key}.key"));
    // the path we hand the game's own open: <mods_dir>\cache\<name>,
    // relative to the game CWD
    let serve = inner.mods_dir_rel.join("cache").join(&name);

    // staleness: rebuild only when the key changed (or the cache is missing)
    let mut fingerprint = Vec::new();
    fingerprint.extend_from_slice(key.as_bytes());
    if key == "csvp" {
        for op in &inner.packs.csvs {
            fingerprint.extend_from_slice(op.mod_id.as_bytes());
            fingerprint.extend_from_slice(op.mod_version.as_bytes());
            fingerprint.extend_from_slice(op.name.as_bytes());
            // row/replace kind + the payload bytes (content-addressed — no
            // file stat needed; the engine already read them at boot)
            fingerprint.push(matches!(op.kind, a9_formats::overlay::CsvOpKind::Rows) as u8);
            fingerprint.extend_from_slice(&(op.payload.len() as u64).to_le_bytes());
            fingerprint.extend_from_slice(&op.payload);
            for k in &op.row_keys {
                fingerprint.extend_from_slice(k.as_bytes());
                fingerprint.push(0);
            }
        }
    } else if key == "i.dat" {
        for op in &inner.packs.assets {
            fingerprint.extend_from_slice(op.mod_id.as_bytes());
            fingerprint.extend_from_slice(op.mod_version.as_bytes());
            fingerprint.extend_from_slice(op.host.as_bytes());
            fingerprint.extend_from_slice(op.name.as_bytes());
            fingerprint.extend_from_slice(op.dir.as_bytes());
            fingerprint.push(op.override_vanilla as u8);
            let (m, s) = stat_key(&op.mod_dir.join(&op.file));
            fingerprint.extend_from_slice(&m.to_le_bytes());
            fingerprint.extend_from_slice(&s.to_le_bytes());
        }
        for l in &inner.packs.listings {
            fingerprint.extend_from_slice(b"listing:");
            fingerprint.extend_from_slice(l.name.as_bytes());
            fingerprint.extend_from_slice(&(l.payload.len() as u64).to_le_bytes());
            fingerprint.extend_from_slice(&l.payload);
        }
    } else if let Some(ops) = inner.packs.images.get(key) {
        for op in ops {
            fingerprint.extend_from_slice(op.mod_id.as_bytes());
            fingerprint.extend_from_slice(op.mod_version.as_bytes());
            fingerprint.extend_from_slice(op.name.as_bytes());
            fingerprint.extend_from_slice(&op.width.to_le_bytes());
            fingerprint.extend_from_slice(&op.height.to_le_bytes());
            let (m, s) = stat_key(&op.mod_dir.join(&op.file));
            fingerprint.extend_from_slice(&m.to_le_bytes());
            fingerprint.extend_from_slice(&s.to_le_bytes());
        }
    } else {
        // a family-A [[asset]] host pack (assets, then any auto-merged listing)
        for op in inner.packs.assets.iter().filter(|a| a.host == key) {
            fingerprint.extend_from_slice(op.mod_id.as_bytes());
            fingerprint.extend_from_slice(op.mod_version.as_bytes());
            fingerprint.extend_from_slice(op.name.as_bytes());
            fingerprint.extend_from_slice(op.dir.as_bytes());
            let (m, s) = stat_key(&op.mod_dir.join(&op.file));
            fingerprint.extend_from_slice(&m.to_le_bytes());
            fingerprint.extend_from_slice(&s.to_le_bytes());
        }
        for l in inner.packs.listings.iter().filter(|l| l.host == key) {
            fingerprint.extend_from_slice(b"listing:");
            fingerprint.extend_from_slice(l.name.as_bytes());
            fingerprint.extend_from_slice(&(l.payload.len() as u64).to_le_bytes());
            fingerprint.extend_from_slice(&l.payload);
        }
    }
    let fp = fnv(&fingerprint);
    let expected_key = format!("{fp:016x}\n");
    if cache_path.is_file()
        && std::fs::read_to_string(&key_path).map(|k| k == expected_key).unwrap_or(false)
    {
        return Ok((serve, true)); // fresh cache — reuse
    }

    // merge (image/asset payloads are read from the mod dirs here; csv payloads
    // already arrived in the plan; the merge cost only happens on changed mod
    // sets — M4's pack-tree insert removes it for the image family)
    let bytes = if key == "csvp" {
        let ops: Vec<a9_formats::overlay::CsvOp> = inner
            .packs
            .csvs
            .iter()
            .map(|op| a9_formats::overlay::CsvOp {
                name: op.name.clone(),
                kind: op.kind,
                payload: op.payload.clone(),
            })
            .collect();
        let tables = {
            let mut v: Vec<&str> = inner.packs.csvs.iter().map(|o| o.name.as_str()).collect();
            v.sort_unstable();
            v.dedup();
            v.len()
        };
        let (b, report) = a9_formats::overlay::overlay_csvp(&original, &ops, BUDGET)
            .map_err(|e| format!("merge: {e}"))?;
        let rows = if report.row_replaced + report.row_appended > 0 {
            format!(
                ", {} row patch(es) ({} replaced, {} added)",
                report.row_replaced + report.row_appended,
                report.row_replaced,
                report.row_appended
            )
        } else {
            String::new()
        };
        inner.logger.line(&format!(
            "overlay csvp: {} table(s) merged ({} added, {} replaced{rows}; {:.1} MB decoded)",
            tables,
            report.added,
            report.replaced,
            report.decoded as f64 / (1024.0 * 1024.0),
        ));
        b
    } else if key == "i.dat" {
        merge_idat(inner, &original)?
    } else if inner.packs.images.contains_key(key) {
        let ops: Vec<ImageOp> = inner
            .packs
            .images
            .get(key)
            .map(|ops| {
                ops.iter()
                    .map(|op| {
                        let payload = std::fs::read(op.mod_dir.join(&op.file)).map_err(|e| {
                            format!("mod {} entry {}: {e}", op.mod_id, op.file.display())
                        })?;
                        Ok(ImageOp {
                            name: op.name.clone(),
                            width: op.width,
                            height: op.height,
                            payload,
                        })
                    })
                    .collect::<Result<Vec<_>, String>>()
            })
            .unwrap_or(Ok(Vec::new()))?;
        let (b, report) = a9_formats::overlay::overlay_pack_b(&original, &ops, BUDGET)
            .map_err(|e| format!("merge: {e}"))?;
        inner.logger.line(&format!(
            "overlay {key}: {} entr{} merged ({} added, {} replaced; {:.1} MB decoded)",
            ops.len(),
            if ops.len() == 1 { "y" } else { "ies" },
            report.added,
            report.replaced,
            report.decoded as f64 / (1024.0 * 1024.0),
        ));
        b
    } else {
        merge_family_a_host(inner, key, &original)?
    };

    // atomic write: tmp + rename
    let tmp = cache_dir.join(format!("{key}.tmp"));
    std::fs::write(&tmp, &bytes).map_err(|e| format!("writing cache: {e}"))?;
    std::fs::rename(&tmp, &cache_path).map_err(|e| format!("renaming cache: {e}"))?;
    std::fs::write(&key_path, expected_key).map_err(|e| format!("writing cache key: {e}"))?;
    Ok((serve, false))
}

/// The full ordered payload list for one family-A host pack: its `[[asset]]`
/// ops (payloads read from the mod dirs) followed by its auto-merged `.dir`
/// listings. The pack merge and the i.dat rows both take their offsets from
/// this one list, so the two can never disagree.
fn host_ops(inner: &Inner, host: &str) -> Result<Vec<a9_formats::overlay::AssetOp>, String> {
    let mut ops: Vec<a9_formats::overlay::AssetOp> = inner
        .packs
        .assets
        .iter()
        .filter(|a| a.host == host)
        .map(|a| {
            let payload = std::fs::read(a.mod_dir.join(&a.file)).map_err(|e| {
                format!("mod {} asset {}: {e}", a.mod_id, a.file.display())
            })?;
            Ok(a9_formats::overlay::AssetOp {
                name: a.name.clone(),
                dir: a.dir.clone(),
                payload,
            })
        })
        .collect::<Result<_, String>>()?;
    // the listings ALWAYS follow the assets, in listing order — the layout the
    // tailgate planner uses too
    ops.extend(
        inner
            .packs
            .listings
            .iter()
            .filter(|l| l.host == host)
            .map(|l| a9_formats::overlay::AssetOp {
                name: l.name.clone(),
                dir: l.dir.clone(),
                payload: l.payload.clone(),
            }),
    );
    Ok(ops)
}

/// The family-A host-pack merge: append the host's `[[asset]]` payloads and its
/// auto-merged `.dir` listings to the decoded stream
/// (`merged = V ++ P0 .. Pn`), re-encode, budget-checked.
fn merge_family_a_host(inner: &Inner, key: &str, original: &[u8]) -> Result<Vec<u8>, String> {
    let ops = host_ops(inner, key)?;
    let (b, report) = a9_formats::overlay::overlay_pack_a(original, &ops, BUDGET)
        .map_err(|e| format!("merge: {e}"))?;
    let listings = ops.len() - inner.packs.assets.iter().filter(|a| a.host == key).count();
    let listing_note = if listings > 0 {
        format!(" + {listings} listing(s)")
    } else {
        String::new()
    };
    inner.logger.line(&format!(
        "overlay {key}: {} asset{} appended ({} row{}{listing_note}; {:.1} MB decoded)",
        ops.len() - listings,
        if ops.len() - listings == 1 { "" } else { "s" },
        ops.len(),
        if ops.len() == 1 { "" } else { "s" },
        report.decoded as f64 / (1024.0 * 1024.0),
    ));
    Ok(b)
}

/// The M5 W1 master-index merge. Order is load-bearing for atomicity:
/// 1. read + parse the shipped `i.dat`;
/// 2. refuse unresolved name collisions — a name that exists in the shipped
///    index without `override_vanilla` (**the authoritative check**: this is
///    the one place the parsed vanilla table is in hand);
/// 3. build every referenced host pack's cache (any failure refuses this
///    merge too — no row may point past a vanilla image);
/// 4. synthesize rows from the same `plan_family_a` the host merges used;
/// 5. serialize + family-A re-encode.
fn merge_idat(inner: &Inner, original: &[u8]) -> Result<Vec<u8>, String> {
    // 1. the shipped index
    let decoded = a9_formats::pack::decode_pack(original, a9_formats::pack::Family::A)
        .map_err(|e| format!("decoding the shipped i.dat: {e}"))?;
    let mut idat = a9_formats::idat::parse(&decoded)
        .map_err(|e| format!("parsing the shipped i.dat: {e}"))?;

    // 2. vanilla collisions — refused before any host pack is built
    for op in &inner.packs.assets {
        if idat.row(&op.name).is_some() && !op.override_vanilla {
            return Err(format!(
                "asset {:?} (mod {}) collides with a SHIPPED i.dat row — set \
                 `override_vanilla = true` on its [[asset]] section to replace it \
                 (the master map's insert-or-find would otherwise silently \
                 overwrite the shipped asset)",
                op.name, op.mod_id
            ));
        }
    }

    // 3. host packs first, in first-appearance order (dedup); a host the
    //    tailgate serves needs no cache built — its payloads are appended in
    //    place at the decode (and a host failure there refuses its own append
    //    without a cache to dangle)
    let mut hosts: Vec<String> = Vec::new();
    let note_host = |hosts: &mut Vec<String>, h: &str| {
        if !hosts.iter().any(|x| x == h) {
            hosts.push(h.to_string());
        }
    };
    for op in &inner.packs.assets {
        note_host(&mut hosts, &op.host);
    }
    for l in &inner.packs.listings {
        note_host(&mut hosts, &l.host);
    }
    for host in &hosts {
        if inner.packs.tailgated.contains(host) {
            continue;
        }
        ensure_cache(inner, host).map_err(|e| format!("host pack {host}: {e}"))?;
    }

    // 4. rows — off from the shared planner, the same ordered op list the
    //    host merges used. A tailgated host's base is its vanilla DECLARED
    //    size (the value the runtime stub asserts); a merged host's is its
    //    decoded length (what the re-encoded cache actually carries). The two
    //    are equal for vanilla packs, so the rows agree either way.
    let mut rows: Vec<a9_formats::idat::NewRow> = Vec::new();
    for host in &hosts {
        let base = if inner.packs.tailgated.contains(host) {
            *inner.packs.declared.get(host).ok_or_else(|| {
                format!("tailgated host {host} has no boot-time declared size")
            })?
        } else {
            let host_path = inner.exe_dir.join("data").join(format!("{host}.bin"));
            let host_file = std::fs::read(&host_path)
                .map_err(|e| format!("reading the host {host}: {e}"))?;
            a9_formats::pack::decode_pack(&host_file, a9_formats::pack::Family::A)
                .map_err(|e| format!("decoding the host {host}: {e}"))?
                .len()
        };
        let ops = host_ops(inner, host)?;
        let plan = a9_formats::overlay::plan_family_a(base, &ops);
        for (op, (off, len)) in ops.iter().zip(plan) {
            rows.push(a9_formats::idat::NewRow {
                name: op.name.clone(),
                dir: op.dir.clone(),
                len: len as u32,
                flag: 0,
                off: off as u32,
            });
        }
    }

    // 5. apply + serialize + re-encode
    let replaced = rows.iter().filter(|r| idat.row(&r.name).is_some()).count();
    idat.add_rows(&rows).map_err(|e| format!("i.dat rows: {e}"))?;
    let out = a9_formats::idat::serialize(&idat);
    let file = a9_formats::pack::encode_pack_a(&out);
    inner.logger.line(&format!(
        "overlay i.dat: {} row{} merged ({} new, {} replaced; {} host pack{}; {:.1} MB decoded)",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" },
        rows.len() - replaced,
        replaced,
        hosts.len(),
        if hosts.len() == 1 { "" } else { "s" },
        out.len() as f64 / (1024.0 * 1024.0),
    ));
    Ok(file)
}
