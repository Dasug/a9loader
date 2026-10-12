//! The M2 boot engine — the general loader. Everything here runs inside
//! `DllMain(DLL_PROCESS_ATTACH)`: **file IO + memory only, never
//! `LoadLibrary`** (loader lock; the real VERSION.dll stays lazily loaded by
//! the forwarders on the game's first version-API call, after init).
//!
//! Flow: config → log → build identity (from the exe FILE — the version APIs
//! are our own forwarded exports and must not be called here) →
//! address-book gate → scan mods → resolve (deps/conflicts/overlaps) →
//! execute effects with expect-verified, journaled writes → summary. The
//! in-image build fingerprint is the per-anchor `expect` verification on
//! every anchored write (a mapped image that doesn't match the book refuses
//! loudly); there is deliberately no banner read. Any failure degrades to
//! "no effects" + a loud log line; the game always boots.
//!
//! Legacy note: `atrain9_proxy.ini` support (including auto-migration) was
//! dropped before anything shipped — the ini never reached any user; mods
//! are created as `mods/<id>/mod.toml` directly.

use crate::{handlers, log, sys};
use a9_mod::{buildid, builds, config, manifest::Manifest, resolve, scan};
use std::path::{Path, PathBuf};

pub const LOADER_VERSION: &str = env!("CARGO_PKG_VERSION");

pub unsafe fn boot(hinst: sys::HMODULE) {
    unsafe { sys::DisableThreadLibraryCalls(hinst) };
    let Ok(exe) = std::env::current_exe() else { return };
    let Some(exe_dir) = exe.parent().map(Path::to_path_buf) else { return };

    let cfg = config::read(&exe_dir.join("a9loader.toml"));
    let mods_dir = exe_dir.join(&cfg.mods_dir);
    let logger = log::Logger::new(&mods_dir.join("a9loader.log"));
    logger.line(&format!("---- a9loader {LOADER_VERSION} (M2 engine) run ----"));
    for w in &cfg.warnings {
        logger.warn(w);
    }
    logger.line(&format!(
        "config: mods_dir \"{}\", conflicts={}, log={:?}{}",
        cfg.mods_dir.display(),
        if cfg.allow_conflicts { "warn+load" } else { "error" },
        cfg.log_level,
        if exe_dir.join("a9loader.toml").is_file() {
            ""
        } else {
            " (defaults — no a9loader.toml)"
        },
    ));

    // ---- build identity (from the exe file; no version.dll APIs in DllMain)
    let Ok(exe_bytes) = std::fs::read(&exe) else {
        logger.err("cannot read the exe image for build identification — no memory effects");
        return;
    };
    let Some(build) = buildid::from_exe_bytes(&exe_bytes) else {
        logger.err("exe has no VERSIONINFO — no memory effects (vanilla boot)");
        return;
    };
    logger.line(&format!(
        "exe build id {} (FILEVERSION {})",
        build.id,
        build.tuple()
    ));
    let Ok(book) = builds::embedded_book() else {
        logger.err("the embedded address book failed to parse — no memory effects");
        return;
    };
    let Some(entry) = book.build.get(&build.id) else {
        logger.err(&format!(
            "unknown build {} — no memory effects (vanilla boot); this loader supports: {}",
            build.id,
            book.build.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
        return;
    };

    // ---- scan + resolve
    let scanned = scan::scan(&mods_dir);
    let mut manifests: Vec<Manifest> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for s in &scanned {
        match &s.manifest {
            Ok(m) => {
                logger.line(&format!(
                    "mod: {} v{} by {} ({} patch, {} hook)",
                    m.id,
                    m.version,
                    m.author,
                    m.patch.len(),
                    m.hook.len()
                ));
                manifests.push(m.clone());
                dirs.push(s.dir.clone());
            }
            Err(e) => logger.err(&format!("{}: {e}", s.dir.display())),
        }
    }
    if manifests.is_empty() {
        logger.line("mods: none — pure proxy");
        return;
    }

    let res = resolve::resolve(&manifests, &cfg.resolve_config());
    for w in &res.warnings {
        logger.warn(w);
    }
    for s in &res.skipped {
        logger.warn(&format!("mod '{}' skipped: {}", s.id, s.reason));
    }
    for o in &res.overlaps {
        let owners: Vec<String> = o
            .owners
            .iter()
            .map(|&i| manifests[i].id.clone())
            .collect();
        logger.err(&format!(
            "overlap on {}: {} — the earlier mod keeps it, later effects refused",
            o.key,
            owners.join(" vs ")
        ));
    }

    // ---- execute
    let mut applied = 0usize;
    let mut refused = 0usize;
    for &i in &res.order {
        let m = &manifests[i];
        let dir = &dirs[i];
        if !m.game_builds.is_empty() && !m.game_builds.iter().any(|b| b == &build.id) {
            logger.err(&format!(
                "mod '{}': game_builds {:?} excludes running build {} — memory effects refused",
                m.id, m.game_builds, build.id
            ));
            refused += m.patch.len() + m.hook.len();
            continue;
        }
        for p in &m.patch {
            let key = format!("patch:{}", p.target.key(p.offset));
            if res.effect_refused(i, &key) {
                logger.err(&format!("mod '{}': {key} refused (overlap loser)", m.id));
                refused += 1;
                continue;
            }
            match unsafe { handlers::apply_patch(p, entry, base_ptr()) } {
                Ok(journal) => {
                    logger.line(&format!("mod '{}': {journal}", m.id));
                    applied += 1;
                }
                Err(e) => {
                    logger.err(&format!("mod '{}': {key} refused: {e}", m.id));
                    refused += 1;
                }
            }
        }
        for h in &m.hook {
            let key = format!("hook:{}", h.target.key(0));
            if res.effect_refused(i, &key) {
                logger.err(&format!("mod '{}': {key} refused (overlap loser)", m.id));
                refused += 1;
                continue;
            }
            match unsafe { handlers::apply_hook(h, entry, base_ptr(), dir) } {
                Ok(journal) => {
                    logger.line(&format!("mod '{}': {journal}", m.id));
                    applied += 1;
                }
                Err(e) => {
                    logger.err(&format!("mod '{}': {key} refused: {e}", m.id));
                    refused += 1;
                }
            }
        }
    }
    logger.line(&format!(
        "result: {applied} effect(s) applied, {refused} refused — game continues"
    ));
}

/// The game exe's mapped image base (GetModuleHandle(NULL) — ASLR-safe with
/// book RVAs).
unsafe fn base_ptr() -> *const u8 {
    unsafe { sys::GetModuleHandleW(std::ptr::null()) as *const u8 }
}
