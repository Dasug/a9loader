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

    // ---- the loader-off escape hatch: enabled = false boots fully vanilla
    // (the proxy stays, everything else is skipped — no scan, no effects,
    // no hooks). The escape hatch reads the SAME toml; flipping is a one-line
    // edit or a `--disable-...`-style wrapper for Steam launch options later.
    if !cfg.enabled {
        logger.line("loader disabled by config (enabled = false) — pure proxy passthrough");
        return;
    }

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

    // ---- scan + resolve (disabled ids are excluded with one line each —
    // a disabled mod is user intent, not an error)
    let scanned = scan::scan(&mods_dir);

    let mut manifests: Vec<Manifest> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for s in &scanned {
        match &s.manifest {
            Ok(m) if cfg.disabled.contains(&m.id) => {
                logger.line(&format!("mod: {} — disabled in a9loader.toml, skipped", m.id));
                continue;
            }
            Ok(m) => {
                logger.line(&format!(
                    "mod: {} v{} by {} ({} patch, {} hook, {} pack entr{}, {} csv, {} file)",
                    m.id,
                    m.version,
                    m.author,
                    m.patch.len(),
                    m.hook.len(),
                    m.pack.iter().map(|p| p.entry.len()).sum::<usize>(),
                    if m.pack.iter().map(|p| p.entry.len()).sum::<usize>() == 1 {
                        "y"
                    } else {
                        "ies"
                    },
                    m.csv.len(),
                    m.file.len()
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
    // ---- data effects (M3/M4): [[pack]]/[[csv]] serving + [[file]] boot copies
    // pack_serving == "inject" routes family-B [[pack]] effects through the
    // M4 window-scoped injector instead of the merge overlay; [[csv]] keeps
    // the M3 merge mechanism in both modes; [[file]] copies are unaffected.
    let mut plan = crate::overlay::Plan {
        images: std::collections::BTreeMap::new(),
        csvs: Vec::new(),
    };
    let inject_mode = cfg.pack_serving == a9_mod::config::PackServing::Inject;
    let mut inject_plan = crate::injector::InjectorPlan::default();
    let mut file_copies: Vec<(String, a9_mod::manifest::FileSection, PathBuf)> = Vec::new();
    for &i in &res.order {
        let m = &manifests[i];
        let dir = &dirs[i];
        for p in &m.pack {
            // only family-B image packs carry entries (me*/mt*); family-A
            // packs (tp/mcp/ep/cp/vp) are size-prefixed streams with no
            // entry table, and csvp tables belong in [[csv]]
            if !matches!(
                a9_formats::pack::classify(&p.pack),
                Some((a9_formats::pack::Family::B, a9_formats::pack::Container::Images))
            ) {
                logger.err(&format!(
                    "mod '{}': pack {} is not a family-B image pack — [[pack]] entries \
                     unsupported there (family-A stream or [[csv]] territory); skipped",
                    m.id, p.pack
                ));
                refused += 1;
                continue;
            }
            for e in &p.entry {
                let key = format!("pack:{}:{}", p.pack, e.name);
                if res.effect_refused(i, &key) {
                    logger.err(&format!("mod '{}': {key} refused (overlap loser)", m.id));
                    refused += 1;
                    continue;
                }
                let target = if inject_mode {
                    inject_plan.packs.entry(p.pack.clone()).or_default()
                } else {
                    plan.images.entry(p.pack.clone()).or_default()
                };
                target.push((dir.clone(), m.id.clone(), m.version.to_string(), e.clone()));
            }
        }
        for c in &m.csv {
            let key = format!("csv:{}", c.name);
            if res.effect_refused(i, &key) {
                logger.err(&format!("mod '{}': {key} refused (overlap loser)", m.id));
                refused += 1;
                continue;
            }
            plan.csvs
                .push((dir.clone(), m.id.clone(), m.version.to_string(), c.clone()));
        }
        for f in &m.file {
            let key = format!("file:{}", f.dest);
            if res.effect_refused(i, &key) {
                logger.err(&format!("mod '{}': {key} refused (overlap loser)", m.id));
                refused += 1;
                continue;
            }
            file_copies.push((m.id.clone(), f.clone(), dir.clone()));
        }
    }

    // [[file]] copies — genuinely-new files, before the game enumerates
    // anything (we run before main); idempotent, never overwrites
    for (mod_id, f, mod_dir) in &file_copies {
        copy_file_effect(mod_id, f, mod_dir, &exe_dir, &mods_dir, &logger);
    }

    // ---- the M4 inject splice (before the overlay hook — a failed splice
    // must fall back to merge BEFORE the overlay plan is finalized). The
    // splice is the ONLY thing inject mode installs: it rewrites the
    // expect-verified family-B runner call over to the injector stub; the
    // stub itself runs per pack window at runtime. Any failure is fail-open:
    // the packs return to the merge plan (today's behavior) untouched.
    let mut injector_live = false;
    if inject_mode && !inject_plan.is_empty() {
        let site_anchor = entry.anchor("famb_runner_call");
        let cave_anchor = entry.anchor("famb_runner_cave");
        match (site_anchor, cave_anchor) {
            (Some(anchor), Some(cave_anchor)) => {
                match (
                    anchor.expect_bytes(),
                    cave_anchor.expect_bytes(),
                ) {
                    (Some(expect), Some(cave_expect))
                        if !expect.is_empty() && !cave_expect.is_empty() =>
                    {
                        match unsafe {
                            crate::injector::install(
                                &mut inject_plan,
                                base_ptr(),
                                anchor.rva as usize,
                                &expect,
                                cave_anchor.rva as usize,
                                &cave_expect,
                                log::Logger::new(&mods_dir.join("a9loader.log")),
                            )
                        } {
                            Ok(()) => injector_live = true,
                            Err(e) => {
                                logger.err(&format!(
                                    "inject refused: famb_runner_call rva={:#x}: {e} — \
                                     [[pack]] effects fall back to merge serving",
                                    anchor.rva
                                ));
                                for (pack, ops) in std::mem::take(&mut inject_plan).packs {
                                    plan.images.entry(pack).or_default().extend(ops);
                                }
                            }
                        }
                    }
                    _ => logger.err(
                        "inject refused: famb anchors have no expect bytes in the \
                         address book — [[pack]] effects fall back to merge",
                    ),
                }
            }
            _ => logger.err(
                "inject refused: this build lacks the famb_runner_call or \
                 famb_runner_cave anchor (older address book?) — [[pack]] \
                 effects fall back to merge",
            ),
        }
    }

    // the overlay hook — one CFile::Open hook serves every matched pack; in
    // inject mode (with the plan moved out) it doubles as the injector's
    // pack-identity capture seam
    if !plan.images.is_empty() || !plan.csvs.is_empty() || injector_live {
        match entry.anchor("cfile_open") {
            Some(anchor) => {
                let image_count: usize = plan.images.values().map(|v| v.len()).sum();
                let csv_count = plan.csvs.len();
                let packs: Vec<String> = plan
                    .images
                    .keys()
                    .cloned()
                    .chain(if plan.csvs.is_empty() {
                        Vec::new()
                    } else {
                        vec!["csvp".to_string()]
                    })
                    .collect();
                match unsafe {
                    crate::overlay::install(
                        plan,
                        cfg.mods_dir.clone(),
                        exe_dir.clone(),
                        anchor.rva as usize,
                        &anchor.expect_bytes().unwrap_or_default(),
                        log::Logger::new(&mods_dir.join("a9loader.log")),
                        cfg.log_level >= a9_mod::config::LogLevel::Debug,
                    )
                } {
                    Ok(())
                        if injector_live && image_count == 0 && csv_count == 0 =>
                    {
                        logger.line(&format!(
                            "overlay: cfile_open hooked rva={:#x} \
                             expect=verified (inject capture hook — no merge \
                             effects served through it)",
                            anchor.rva
                        ));
                    }
                    Ok(()) => logger.line(&format!(
                        "overlay: cfile_open hooked rva={:#x} expect=verified ({image_count} entr{} \
                         across packs: {}; {csv_count} csv table{})",
                        anchor.rva,
                        if image_count == 1 { "y" } else { "ies" },
                        packs.join(", "),
                        if csv_count == 1 { "" } else { "s" },
                    )),
                    Err(e) => {
                        logger.err(&format!(
                            "overlay refused: cfile_open rva={:#x}: {e}",
                            anchor.rva
                        ));
                        if injector_live {
                            logger.err(
                                "inject serving cannot fire without the capture \
                                 hook — [[pack]] inject effects serve vanilla \
                                 this boot",
                            );
                        }
                    }
                }
            }
            None => {
                logger.err("overlay refused: this build has no cfile_open anchor");
                if injector_live {
                    logger.err(
                        "inject serving cannot fire without the capture hook — \
                         [[pack]] inject effects serve vanilla this boot",
                    );
                }
            }
        }
    }

    // ---- M4 fetch probe (opt-in diagnostics; trampoline-only, fail-open)
    // Installs after the build gate + overlay block; each anchor
    // expect-verifies like every other write. Pure diagnostics: nothing
    // downstream changes behavior, all failure paths log loudly and the
    // game boots vanilla-booted.
    if cfg.probe_fetch {
        logger.line("probe: fetch hooks on (diagnostic)");
        let mut specs: Vec<crate::probe::Spec> = Vec::new();
        for name in crate::probe::PROBE_ANCHORS {
            match entry.anchor(name) {
                Some(a) => match a.expect_bytes() {
                    Some(expect) if !expect.is_empty() => {
                        specs.push(crate::probe::Spec {
                            name,
                            rva: a.rva as usize,
                            expect,
                        });
                    }
                    _ => logger.err(&format!(
                        "probe refused: anchor {name} has no expect bytes in the address book"
                    )),
                },
                None => logger.err(&format!(
                    "probe refused: this build has no {name} anchor (older address book?)"
                )),
            }
        }
        if let Err(e) =
            unsafe { crate::probe::install(log::Logger::new(&mods_dir.join("a9loader.log")), base_ptr(), &specs) }
        {
            logger.err(&format!("probe: {e} — remaining boot unaffected"));
        }
    }

    logger.line(&format!(
        "result: {applied} memory effect(s) applied, {refused} refused, {} file cop{} — game continues",
        file_copies.len(),
        if file_copies.len() == 1 { "y" } else { "ies" },
    ));
}

/// `[[file]]` — copy a genuinely-new file into the game tree, journaled to
/// `<mods_dir>/journal.json` (JSON lines). Idempotent: identical content is
/// a no-op; a differing existing file refuses loudly (never overwrites —
/// replacing shipped content is [[pack]]/[[csv]] territory).
fn copy_file_effect(
    mod_id: &str,
    f: &a9_mod::manifest::FileSection,
    mod_dir: &Path,
    exe_dir: &Path,
    mods_dir: &Path,
    logger: &log::Logger,
) {
    let dest = exe_dir.join(&f.dest);
    let src = mod_dir.join(&f.file);
    let payload = match std::fs::read(&src) {
        Ok(b) => b,
        Err(e) => {
            logger.err(&format!(
                "mod '{mod_id}': file {} -> {}: source unreadable ({e}) — skipped",
                f.file,
                f.dest
            ));
            return;
        }
    };
    if dest.exists() {
        match std::fs::read(&dest) {
            Ok(existing) if existing == payload => {
                logger.line(&format!(
                    "mod '{mod_id}': file {} already in place (idempotent)",
                    f.dest
                ));
                return;
            }
            _ => {
                logger.err(&format!(
                    "mod '{mod_id}': file dest {} exists with different content — \
                     refusing to overwrite (delete it, or replace content via [[pack]]/[[csv]])",
                    f.dest
                ));
                return;
            }
        }
    }
    if let Some(parent) = dest.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            logger.err(&format!(
                "mod '{mod_id}': file {} -> {}: cannot create {}",
                f.file,
                f.dest,
                parent.display()
            ));
            return;
        }
    }
    match std::fs::write(&dest, &payload) {
        Ok(()) => {
            logger.line(&format!(
                "mod '{mod_id}': file {} -> {} copied (journaled)",
                f.file,
                f.dest
            ));
            journal_append(mods_dir, mod_id, f);
        }
        Err(e) => logger.err(&format!(
            "mod '{mod_id}': file {} -> {}: copy failed ({e})",
            f.file,
            f.dest
        )),
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn journal_append(mods_dir: &Path, mod_id: &str, f: &a9_mod::manifest::FileSection) {
    // JSON lines — no array juggling, appends stay atomic under a lock-free
    // single-writer boot
    let line = format!(
        "{{\"mod\":\"{}\",\"file\":\"{}\",\"dest\":\"{}\"}}\n",
        json_escape(mod_id),
        json_escape(&f.file),
        json_escape(&f.dest)
    );
    let path = mods_dir.join("journal.json");
    use std::io::Write;
    if let Ok(mut fh) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = fh.write_all(line.as_bytes());
    }
}

/// The game exe's mapped image base (GetModuleHandle(NULL) — ASLR-safe with
/// book RVAs).
unsafe fn base_ptr() -> *const u8 {
    unsafe { sys::GetModuleHandleW(std::ptr::null()) as *const u8 }
}
