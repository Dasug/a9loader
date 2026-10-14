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
//!
//! This module holds the plan handover, the install and the hook dispatcher;
//! the merge itself lives in [`merge`] and the `[[loose_asset]]` table in
//! [`loose`].

mod loose;
mod merge;

use crate::{log, patch, sys};
use std::collections::BTreeMap;
use std::path::PathBuf;

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
    loose: Vec<loose::LooseEntry>,
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
        let loose = loose::materialize_loose(&plan.loose, &mods_dir_abs, &mods_dir, &logger);
        let packs = Packs::from_plan(plan, loose);
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

impl Packs {
    /// Fold the engine's plan handover into the runtime op tables: image entry
    /// ops, csvp table ops, `[[asset]]` ops, `.dir` listings, the tailgated
    /// set, the per-host declared sizes, and the materialized loose table.
    fn from_plan(plan: Plan, loose: Vec<loose::LooseEntry>) -> Packs {
        Packs {
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
        }
    }
}

pub(super) type CFileOpenFn =
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
        let Some(inner) = G.get() else {
            return 1; // not installed — cannot happen (we ARE the hook)
        };
        let original: CFileOpenFn = core::mem::transmute(inner.original.as_ptr());
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
        if let Some(served) = loose::try_serve(inner, &norm, original, this, flags, perr) {
            return served;
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
        let cache = match merge::ensure_cache(inner, &key) {
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
pub(super) fn pack_file_name(key: &str) -> String {
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
