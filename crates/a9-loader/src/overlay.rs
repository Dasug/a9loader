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

/// One csvp table op.
struct PendingCsvOp {
    mod_id: String,
    mod_version: String,
    name: String,
    mod_dir: PathBuf,
    file: PathBuf,
}

#[derive(Default)]
struct Packs {
    /// family-B image packs (me*/mt*): pack name → ops (priority order)
    images: BTreeMap<String, Vec<PendingImageOp>>,
    /// the 0x204 csv container (csvp): table ops (priority order)
    csvs: Vec<PendingCsvOp>,
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

/// The plan the engine hands over at boot. Consumed by [`install`].
pub struct Plan {
    /// `data\<pack>.bin` name (e.g. "mtentr0") → entry ops in priority order,
    /// each with its owning mod dir (payloads are mod-relative)
    pub images: BTreeMap<String, Vec<(PathBuf, String, String, a9_mod::manifest::PackEntry)>>,
    /// csvp table ops in priority order
    pub csvs: Vec<(PathBuf, String, String, a9_mod::manifest::CsvSection)>,
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
                .map(|(dir, id, ver, c)| PendingCsvOp {
                    mod_id: id,
                    mod_version: ver,
                    name: c.name,
                    mod_dir: dir,
                    file: PathBuf::from(&c.file),
                })
                .collect(),
        };
        let _ = G.set(Inner {
            packs,
            mods_dir: exe_dir.join(&mods_dir),
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

/// `data\mtentr0.bin` (or a bare `mtentr0.bin`) → "mtentr0"; `None` = not a
/// pack path at all. Whether the pack is actually OVERLAYED is decided by
/// [`overlay_key`] — this only shapes the path.
fn pack_key(norm: &str) -> Option<String> {
    let (dir, file) = match norm.rfind('\\') {
        Some(i) => (&norm[..i], &norm[i + 1..]),
        None => ("", norm),
    };
    if !dir.is_empty() && dir != "data" {
        return None;
    }
    let pack = file.strip_suffix(".bin")?;
    Some(pack.to_string())
}

/// The pack named by the open, IF the overlay plan touches it — `None` =
/// passthrough untouched (no ops, no rebuild, no cache). Only packs with
/// mod effects are ever redirected; the live-run lesson (2026-10-16): a
/// broader match "merged 0 entries" on every pack the game opened, rebuilt
/// untouched 141 MB packs, and ran the family-B parser over family-A
/// packs (fail-open saved those, loudly).
fn overlay_key(inner: &Inner, pack: &str) -> Option<String> {
    if pack == "csvp" {
        if inner.packs.csvs.is_empty() {
            None
        } else {
            Some(pack.to_string())
        }
    } else {
        inner
            .packs
            .images
            .contains_key(pack)
            .then(|| pack.to_string())
    }
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

/// Build (or reuse) the cache for `key`; returns the cache path relative to
/// the game CWD ("mods\cache\<pack>.bin"). Failures are remembered and
/// repeat as loud-once errors.
fn ensure_cache(inner: &Inner, key: &str) -> Result<PathBuf, String> {
    let mut built = inner
        .built
        .lock()
        .map_err(|_| "cache state poisoned".to_string())?;
    if let Some(r) = built.get(key) {
        return r.clone();
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
    let out = result.clone().map(|(p, _)| p);
    built.insert(key.to_string(), out.clone());
    out
}

/// The merge: read the original pack, apply the ops, write the cache
/// atomically (tmp + rename) + the key file. Returns (serve path,
/// cache-was-fresh-reused).
fn build_cache(inner: &Inner, key: &str) -> Result<(PathBuf, bool), String> {
    let cache_dir = inner.mods_dir.join("cache");
    std::fs::create_dir_all(&cache_dir).map_err(|e| format!("cache dir: {e}"))?;
    let original_path = inner.exe_dir.join("data").join(format!("{key}.bin"));
    let original = std::fs::read(&original_path)
        .map_err(|e| format!("reading the original {}: {e}", original_path.display()))?;

    let cache_path = cache_dir.join(format!("{key}.bin"));
    let key_path = cache_dir.join(format!("{key}.key"));
    // the path we hand the game's own open: <mods_dir>\cache\<pack>.bin,
    // relative to the game CWD
    let serve = inner
        .mods_dir_rel
        .join("cache")
        .join(format!("{key}.bin"));

    // staleness: rebuild only when the key changed (or the cache is missing)
    let mut fingerprint = Vec::new();
    fingerprint.extend_from_slice(key.as_bytes());
    if key == "csvp" {
        for op in &inner.packs.csvs {
            fingerprint.extend_from_slice(op.mod_id.as_bytes());
            fingerprint.extend_from_slice(op.mod_version.as_bytes());
            fingerprint.extend_from_slice(op.name.as_bytes());
            let (m, s) = stat_key(&op.mod_dir.join(&op.file));
            fingerprint.extend_from_slice(&m.to_le_bytes());
            fingerprint.extend_from_slice(&s.to_le_bytes());
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
    }
    let fp = fnv(&fingerprint);
    let expected_key = format!("{fp:016x}\n");
    if cache_path.is_file()
        && std::fs::read_to_string(&key_path).map(|k| k == expected_key).unwrap_or(false)
    {
        return Ok((serve, true)); // fresh cache — reuse
    }

    // merge (payloads are read from the mod dirs here — the merge cost only
    // happens on changed mod sets; M4's pack-tree insert removes it entirely)
    let bytes = if key == "csvp" {
        let ops: Vec<a9_formats::overlay::CsvOp> = inner
            .packs
            .csvs
            .iter()
            .map(|op| {
                let payload = std::fs::read(op.mod_dir.join(&op.file)).map_err(|e| {
                    format!("mod {} table {}: {e}", op.mod_id, op.file.display())
                })?;
                Ok(a9_formats::overlay::CsvOp {
                    name: op.name.clone(),
                    payload,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let (b, report) = a9_formats::overlay::overlay_csvp(&original, &ops, BUDGET)
            .map_err(|e| format!("merge: {e}"))?;
        inner.logger.line(&format!(
            "overlay csvp: {} table(s) merged ({} added, {} replaced; {:.1} MB decoded)",
            ops.len(),
            report.added,
            report.replaced,
            report.decoded as f64 / (1024.0 * 1024.0),
        ));
        b
    } else {
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
    };

    // atomic write: tmp + rename
    let tmp = cache_dir.join(format!("{key}.tmp"));
    std::fs::write(&tmp, &bytes).map_err(|e| format!("writing cache: {e}"))?;
    std::fs::rename(&tmp, &cache_path).map_err(|e| format!("renaming cache: {e}"))?;
    std::fs::write(&key_path, expected_key).map_err(|e| format!("writing cache key: {e}"))?;
    Ok((serve, false))
}
