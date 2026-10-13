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

use crate::{forms, handlers, log, sys};
use a9_mod::{buildid, builds, config, manifest::Manifest, resolve, scan};
use std::collections::{BTreeMap, BTreeSet};
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
        "config: mods_dir \"{}\", conflicts={}, log={:?}, pack_serving={}, asset_serving={}{}",
        cfg.mods_dir.display(),
        if cfg.allow_conflicts { "warn+load" } else { "error" },
        cfg.log_level,
        match cfg.pack_serving {
            a9_mod::config::PackServing::Merge => "merge",
            a9_mod::config::PackServing::Inject => "inject",
        },
        match cfg.asset_serving {
            a9_mod::config::AssetServing::Tailgate => "tailgate",
            a9_mod::config::AssetServing::Merge => "merge",
        },
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
                    "mod: {} v{} by {} ({} patch, {} hook, {} pack entr{}, {} csv, {} asset, {} file)",
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
                    m.asset.len(),
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
        // M5-T is config-driven: an explicit `workspace_out_mb` is honored even
        // with no mods (AUTO has nothing to size against, so it does nothing).
        unsafe {
            let _ = maybe_grow_workspace(&cfg, 0, entry, &logger);
        }
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
    // `train_list` contributions are collected here and composed into ONE
    // filter install after the loop (M5 W3 — several mods may each declare
    // forms to offer/hide; a priority tie must not decide membership).
    let mut train_list: Vec<handlers::Contribution> = Vec::new();
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
            if h.handler == "train_list" {
                match unsafe { handlers::train_list_contribution(h, entry, base_ptr(), dir, &m.id) } {
                    Ok(c) => train_list.push(c),
                    Err(e) => {
                        logger.err(&format!("mod '{}': {key} refused: {e}", m.id));
                        refused += 1;
                    }
                }
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
    // the composed train_list filter — one install for every contribution
    if !train_list.is_empty() {
        let forms_len = unsafe {
            entry
                .anchor("form_list")
                .and_then(|a| {
                    handlers::read_u16s_bounded(base_ptr(), a.rva as usize, 16384).ok()
                })
                .map(|u| forms::parse_form_list(&u).len())
                .unwrap_or(417)
        };
        match unsafe {
            handlers::install_train_list(&train_list, entry, base_ptr(), forms_len, &logger)
        } {
            Ok(journal) => {
                logger.line(&journal);
                applied += 1;
            }
            Err(e) => {
                logger.err(&format!(
                    "train_list: {e} — {} contribution(s) refused",
                    train_list.len()
                ));
                refused += train_list.len();
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
        assets: Vec::new(),
        listings: Vec::new(),
        tailgated: std::collections::BTreeSet::new(),
        declared: std::collections::BTreeMap::new(),
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
            let payload = match std::fs::read(dir.join(&c.file)) {
                Ok(b) => b,
                Err(e) => {
                    logger.err(&format!(
                        "mod '{}': csv {} payload {}: {e} — effect skipped",
                        m.id,
                        c.name,
                        c.file
                    ));
                    refused += 1;
                    continue;
                }
            };
            match c.mode {
                a9_mod::manifest::CsvMode::Replace => {
                    let key = format!("csv:{}", c.name);
                    if res.effect_refused(i, &key) {
                        logger.err(&format!("mod '{}': {key} refused (overlap loser)", m.id));
                        refused += 1;
                        continue;
                    }
                    plan.csvs.push(crate::overlay::CsvOpPlan {
                        mod_id: m.id.clone(),
                        mod_version: m.version.to_string(),
                        kind: a9_formats::overlay::CsvOpKind::Replace,
                        name: c.name.clone(),
                        payload,
                        row_keys: Vec::new(),
                    });
                }
                a9_mod::manifest::CsvMode::Rows => {
                    // whole-table fallback key (the patch file was unreadable at
                    // scan time -> row_keys empty): fail-safe refusal if spent
                    let table_key = format!("csv:{}", c.name);
                    if res.effect_refused(i, &table_key) {
                        logger.err(&format!(
                            "mod '{}': {table_key} refused (overlap loser)",
                            m.id
                        ));
                        refused += 1;
                        continue;
                    }
                    let mut kept = match a9_formats::csv::parse_patch(&payload) {
                        Ok(t) => t,
                        Err(e) => {
                            logger.err(&format!(
                                "mod '{}': csv {} row patch {}: {e} — effect skipped",
                                m.id, c.name, c.file
                            ));
                            refused += 1;
                            continue;
                        }
                    };
                    // per-row overlap refusal: a row another mod already owns
                    // (csv:<table>:<rowkey>) is dropped here, the rest apply
                    let mut dropped = 0usize;
                    let mut keys: Vec<String> = Vec::new();
                    kept.retain_rows(|r| {
                        let k = r.key();
                        if k.is_empty() {
                            return true; // the merge refuses this loudly
                        }
                        if res.effect_refused(i, &format!("csv:{}:{}", c.name, k)) {
                            dropped += 1;
                            false
                        } else {
                            keys.push(k.to_string());
                            true
                        }
                    });
                    if dropped > 0 {
                        logger.err(&format!(
                            "mod '{}': csv {} — {dropped} row patch(es) refused (overlap loser)",
                            m.id, c.name
                        ));
                        refused += dropped;
                    }
                    if kept.is_empty() {
                        continue;
                    }
                    plan.csvs.push(crate::overlay::CsvOpPlan {
                        mod_id: m.id.clone(),
                        mod_version: m.version.to_string(),
                        kind: a9_formats::overlay::CsvOpKind::Rows,
                        name: c.name.clone(),
                        payload: kept.serialize(),
                        row_keys: keys,
                    });
                }
            }
        }
        // [[asset]] — new i.dat rows with payloads appended to a family-A host
        // pack (M5 W1). Always merge-served: inject has no family-A seam.
        // `host = "auto"` (the folder-form default) is resolved here, against
        // the embedded per-build dir index — the boot knows the build.
        let auto_index: Option<Result<a9_mod::dirindex::DirIndex, a9_mod::ModError>> = m
            .asset
            .iter()
            .any(|a| a.host.eq_ignore_ascii_case(a9_mod::manifest::HOST_AUTO))
            .then(a9_mod::dirindex::embedded_index);
        for a in &m.asset {
            let host = if a.host.eq_ignore_ascii_case(a9_mod::manifest::HOST_AUTO) {
                match &auto_index {
                    Some(Ok(idx)) => {
                        match a9_mod::dirindex::resolve_auto(idx, &build.id, &a.name, &a.dir) {
                            Ok(h) => {
                                logger.line(&format!(
                                    "mod '{}': asset {}: host auto -> {h} (dir index)",
                                    m.id, a.name
                                ));
                                h
                            }
                            Err(e) => {
                                logger.err(&format!(
                                    "mod '{}': asset {}: host auto refused for dir {:?}: {e} \
                                     — effect skipped",
                                    m.id, a.name, a.dir
                                ));
                                refused += 1;
                                continue;
                            }
                        }
                    }
                    Some(Err(e)) => {
                        logger.err(&format!(
                            "mod '{}': asset {}: host auto refused — the embedded dir index \
                             failed to parse ({e})",
                            m.id, a.name
                        ));
                        refused += 1;
                        continue;
                    }
                    None => {
                        logger.err(&format!(
                            "mod '{}': asset {}: host auto refused — no dir index for build {}",
                            m.id, a.name, build.id
                        ));
                        refused += 1;
                        continue;
                    }
                }
            } else {
                a.host.clone()
            };
            if !matches!(
                a9_formats::pack::classify(&host),
                Some((a9_formats::pack::Family::A, a9_formats::pack::Container::Opaque))
            ) {
                logger.err(&format!(
                    "mod '{}': asset {} host {} is not a family-A pack the loader \
                     can append to (vp/tp/mcp/ep/cp); skipped",
                    m.id, a.name, host
                ));
                refused += 1;
                continue;
            }
            let key = format!("asset:{}", a.name);
            if res.effect_refused(i, &key) {
                logger.err(&format!(
                    "mod '{}': {key} refused (overlap loser — an earlier mod already \
                     provides this asset name)",
                    m.id
                ));
                refused += 1;
                continue;
            }
            plan.assets.push(crate::overlay::AssetOpPlan {
                mod_id: m.id.clone(),
                mod_version: m.version.to_string(),
                host,
                name: a.name.clone(),
                dir: a.dir.clone(),
                mod_dir: dir.clone(),
                file: a.file.clone(),
                override_vanilla: a.override_vanilla,
            });
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

    // [[asset]] serving is orthogonal to pack_serving: the tailgate (the
    // default) has a family-A seam of its own, so inject mode does not force
    // these onto the merge path. Say which path applies when the two are set
    // to different mechanisms so a user reading the log is not surprised.
    if inject_mode && !plan.assets.is_empty() {
        let how = match cfg.asset_serving {
            a9_mod::config::AssetServing::Tailgate => {
                "the family-A tailgate (in-place decode append — inject is family-B only)"
            }
            a9_mod::config::AssetServing::Merge => {
                "force merge serving for their host packs + data\\i.dat (inject has no \
                 family-A seam)"
            }
        };
        logger.line(&format!(
            "[[asset]]: {} asset(s) serve via {how}",
            plan.assets.len()
        ));
    }

    // ---- M5-U: derive the auto-merged `.dir` group listings from the mods'
    // OWN [[asset]] rows (an author never writes a `.dir` file). Runs before
    // the serving decision: the rebuilt listing payload rides its host pack
    // exactly like an asset payload and its row joins the i.dat rows on
    // whichever path serves that host.
    plan.listings = plan_listings(&plan, &exe_dir, &logger);

    // ---- M5 tailgate: [[asset]] new assets appended in place at the family-A
    // decode (no cache, no re-encode). Decided per host at boot; merge is the
    // fallback for anything that does not line up. Runs BEFORE the overlay
    // install — the overlay must know which packs to leave alone.
    {
        // pass 1: decide against the workspace the game will allocate
        let mut decision = unsafe {
            decide_tailgate(
                &plan,
                &exe_dir,
                entry,
                base_ptr(),
                &logger,
                cfg.asset_serving,
                TAILGATE_OUT_CAP,
            )
        };

        // ---- M5-T: the family-A decode workspace (the 200 MB ceiling). The
        // game allocates it once at boot and bounds the LZSS decoder by its OUT
        // cap, so a mod set whose family-A content does not fit is refused per
        // host by the tailgate's own budget check. When the plan needs more,
        // grow the workspace at the game's alloc tail — before any decode, cap
        // raised, buffer from the game's own `_aligned_malloc` — and re-decide
        // against the grown cap (otherwise an oversized host would "fall back"
        // to a merge path whose 150 MB encoded-file buffer cannot serve it
        // either). AUTO by default; `workspace_out_mb` forces a size.
        // Fail-open: a refusal keeps the game's buffers and says so.
        let needed = plan_family_a_need(&plan, &decision.declared);
        let grown = unsafe { maybe_grow_workspace(&cfg, needed, entry, &logger) };
        if grown > 0 {
            decision = unsafe {
                decide_tailgate(
                    &plan,
                    &exe_dir,
                    entry,
                    base_ptr(),
                    &logger,
                    cfg.asset_serving,
                    grown,
                )
            };
        }

        plan.declared = decision.declared;
        plan.tailgated = decision.tailgated;
        if !decision.note.is_empty() {
            logger.line(&decision.note);
        }
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
    if !plan.images.is_empty()
        || !plan.csvs.is_empty()
        || !plan.assets.is_empty()
        || !plan.listings.is_empty()
        || injector_live
    {
        match entry.anchor("cfile_open") {
            Some(anchor) => {
                let image_count: usize = plan.images.values().map(|v| v.len()).sum();
                let csv_count = {
                    let mut v: Vec<&str> = plan.csvs.iter().map(|c| c.name.as_str()).collect();
                    v.sort_unstable();
                    v.dedup();
                    v.len()
                };
                let asset_count = plan.assets.len();
                let listing_count = plan.listings.len();
                let packs: Vec<String> = plan
                    .images
                    .keys()
                    .cloned()
                    .chain(if plan.csvs.is_empty() {
                        Vec::new()
                    } else {
                        vec!["csvp".to_string()]
                    })
                    .chain(if plan.assets.is_empty() {
                        Vec::new()
                    } else {
                        plan.assets
                            .iter()
                            .map(|a| a.host.clone())
                            .chain(std::iter::once("i.dat".to_string()))
                            .collect::<std::collections::BTreeSet<_>>()
                            .into_iter()
                            .collect()
                    })
                    .collect();
                let asset_note = if asset_count > 0 {
                    let listing = if listing_count > 0 {
                        format!(", {listing_count} extended listing{}", if listing_count == 1 { "" } else { "s" })
                    } else {
                        String::new()
                    };
                    format!(
                        "; {asset_count} asset{}{listing}",
                        if asset_count == 1 { "" } else { "s" }
                    )
                } else {
                    String::new()
                };
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
                        if injector_live
                            && image_count == 0
                            && csv_count == 0
                            && asset_count == 0
                            && listing_count == 0 =>
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
                         across packs: {}; {csv_count} csv table{}{asset_note})",
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

// ---- M5-U: auto-merged `.dir` group listings ----------------------------------

/// Comma-group a list of strings for a log line.
fn join_list(items: &[String]) -> String {
    items.join(", ")
}

/// `1672` → `"1,672"` (the entry-count log line).
fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    let bytes = s.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Build the M5-U auto-merged `.dir` group listings for this boot.
///
/// A mod's `[[asset]]` with `dir = D` is a membership claim on the group whose
/// listing row is `D.dir`. For each distinct group `D` that has a shipped
/// `D.dir` row and is on the auto-merge allow-list
/// ([`a9_formats::overlay::listing_auto_merge`] — only listings proven
/// name-keyed), the loader reads the shipped listing bytes out of the hosting
/// pack, appends the mods' new names (deduped), and returns the rebuilt
/// payload. The serving path then appends it to that host after the host's
/// asset payloads and rewrites the shipped listing row.
///
/// A listing that exists but is NOT auto-merged gets a loud "not auto-merged"
/// line (never a failure) and stays on the explicit `[[asset]] override_vanilla`
/// path. A listing whose bytes do not parse as a UTF-16 name run at that host is
/// refused loudly — the shipped listing stays vanilla.
///
/// The host of a group's listing is the host its assets resolved to (the
/// dirindex answer): for every shipped listing those coincide (previewtexture →
/// tp, base/tree/light/… → mcp), while the listing row's OWN `dir` is ambiguous
/// by construction (memory #315), so the asset host is the reliable key.
fn plan_listings(
    plan: &crate::overlay::Plan,
    exe_dir: &Path,
    logger: &log::Logger,
) -> Vec<crate::overlay::ListingOpPlan> {
    // groups in first-appearance order (the asset list is the resolved order)
    let mut groups: Vec<String> = Vec::new();
    for a in &plan.assets {
        if !groups.iter().any(|g| g == &a.dir) {
            groups.push(a.dir.clone());
        }
    }
    if groups.is_empty() {
        return Vec::new();
    }

    // the shipped index: the authoritative "does D.dir exist" + its (off, len)
    let shipped = std::fs::read(exe_dir.join("data").join("i.dat"))
        .map_err(|e| format!("reading data\\i.dat: {e}"))
        .and_then(|b| {
            a9_formats::pack::decode_pack(&b, a9_formats::pack::Family::A)
                .map_err(|e| format!("decoding data\\i.dat: {e}"))
        })
        .and_then(|d| a9_formats::idat::parse(&d).map_err(|e| format!("parsing data\\i.dat: {e}")));
    let shipped = match shipped {
        Ok(i) => i,
        Err(e) => {
            logger.err(&format!(
                "overlay listings: {e} — no .dir listing can be auto-merged this boot"
            ));
            return Vec::new();
        }
    };

    let mut out: Vec<crate::overlay::ListingOpPlan> = Vec::new();
    // one decode per host, however many listing groups that host carries
    let mut host_cache: BTreeMap<String, Result<Vec<u8>, String>> = BTreeMap::new();
    for dir in &groups {
        let lname = format!("{dir}.dir");
        let Some(row) = shipped.row(&lname) else {
            continue; // not a listing group — the assets just add normally
        };
        let contributors: Vec<&crate::overlay::AssetOpPlan> =
            plan.assets.iter().filter(|a| &a.dir == dir).collect();
        let mut mods: Vec<String> = Vec::new();
        for a in &contributors {
            if !mods.iter().any(|m| m == &a.mod_id) {
                mods.push(a.mod_id.clone());
            }
        }

        if !a9_formats::overlay::listing_auto_merge(&lname) {
            logger.line(&format!(
                "overlay listing: {lname} NOT auto-merged ({} name(s) from mod(s) {}) — this \
                 listing's consumer may be positional, so the loader will not compose it; ship \
                 an explicit [[asset]] name = \"{lname}\" override_vanilla = true to extend it",
                contributors.len(),
                join_list(&mods)
            ));
            continue;
        }
        // an explicit override of the listing row keeps that author in control:
        // serve it byte-for-byte rather than deriving on top of it
        if plan.assets.iter().any(|a| a.name == lname) {
            logger.line(&format!(
                "overlay listing: {lname} not auto-merged (an explicit [[asset]] override of \
                 the row is present) — the override is served byte-for-byte"
            ));
            continue;
        }
        // the group's host must be unambiguous (all its assets resolve to one)
        let host = contributors[0].host.clone();
        if contributors.iter().any(|a| a.host != host) {
            logger.err(&format!(
                "overlay listing: {lname} not auto-merged (its group's assets resolved to \
                 different host packs) — ship an explicit [[asset]] override of the row"
            ));
            continue;
        }
        // read the shipped listing bytes out of the hosting pack (decoded once
        // per host, shared across listing groups on the same host)
        let decoded = host_cache.entry(host.clone()).or_insert_with(|| {
            let host_path = exe_dir.join("data").join(format!("{host}.bin"));
            std::fs::read(&host_path)
                .map_err(|e| format!("reading data\\{host}.bin: {e}"))
                .and_then(|b| {
                    a9_formats::pack::decode_pack(&b, a9_formats::pack::Family::A)
                        .map_err(|e| format!("decoding data\\{host}.bin: {e}"))
                })
        });
        let decoded = match decoded {
            Ok(d) => d,
            Err(e) => {
                logger.err(&format!(
                    "overlay listing: {lname}: {e} — auto-merge refused (the listing stays vanilla)"
                ));
                continue;
            }
        };
        let off = row.v3() as usize;
        let len = row.v1() as usize;
        let Some(vanilla) = decoded.get(off..off.saturating_add(len)) else {
            logger.err(&format!(
                "overlay listing: {lname}: the shipped row (off {off}, len {len}) does not fit \
                 data\\{host}.bin's decoded image ({} B) — auto-merge refused (wrong host?)",
                decoded.len()
            ));
            continue;
        };
        let names: Vec<String> = contributors.iter().map(|a| a.name.clone()).collect();
        match a9_formats::overlay::listing_extend(vanilla, &names) {
            Ok((payload, added, total)) => {
                if added == 0 {
                    logger.debug(&format!(
                        "overlay listing: {lname}: every contributed name is already listed — \
                         no extension"
                    ));
                    continue;
                }
                logger.line(&format!(
                    "overlay listing: {lname} extended by {added} name(s) -> {} entries \
                     (host {host}; mod(s) {})",
                    thousands(total),
                    join_list(&mods)
                ));
                out.push(crate::overlay::ListingOpPlan {
                    host,
                    name: lname,
                    dir: row.dir.clone(),
                    payload,
                });
            }
            Err(e) => logger.err(&format!(
                "overlay listing: {lname} auto-merge refused: {e} — the shipped listing stays \
                 vanilla (ship an explicit [[asset]] override of the row if you need it)"
            )),
        }
    }
    out
}

// ---- M5 tailgate boot decision -------------------------------------------------

/// The engine's decode-workspace ceiling (`DAT_144f86098` out-cap = 0xC800000,
/// 200 MiB): appended bytes must fit under it with the vanilla image.
const TAILGATE_OUT_CAP: usize = 0xC800000;

struct TailgateDecision {
    /// per-host vanilla declared size (the append base == the i.dat row offset)
    declared: BTreeMap<String, usize>,
    /// packs (incl. "i.dat") the tailgate actually serves; the overlay must
    /// leave exactly these alone (no redirect, no cache)
    tailgated: BTreeSet<String>,
    /// the one loud line describing the chosen path (or empty)
    note: String,
}

/// The `pack_lzss_*_call` anchor name for a family-A host pack. `None` = a
/// host this loader can append to but has no tailgate splice site for (it
/// serves through the merge instead).
fn tailgate_site_anchor(host: &str) -> Option<&'static str> {
    match host {
        "vp" => Some("pack_lzss_vp_call"),
        "tp" => Some("pack_lzss_tp_call"),
        "mcp" => Some("pack_lzss_mcp_call"),
        "ep" => Some("pack_lzss_ep_call"),
        "cp" => Some("pack_lzss_cp_call"),
        _ => None,
    }
}

/// The largest family-A host requirement this plan implies: the host's vanilla
/// declared size plus every payload the loader appends to it (`[[asset]]`
/// payloads and the M5-U derived `.dir` listings). `declared` is the map the
/// tailgate decision read at boot, so this is exact whenever that read worked.
fn plan_family_a_need(
    plan: &crate::overlay::Plan,
    declared: &std::collections::BTreeMap<String, usize>,
) -> usize {
    let mut need: std::collections::BTreeMap<String, usize> = declared.clone();
    for a in &plan.assets {
        let sz = std::fs::metadata(a.mod_dir.join(&a.file))
            .map(|m| m.len() as usize)
            .unwrap_or(0);
        *need.entry(a.host.clone()).or_default() += sz;
    }
    for l in &plan.listings {
        *need.entry(l.host.clone()).or_default() += l.payload.len();
    }
    need.values().copied().max().unwrap_or(0)
}

/// M5-T: arm the family-A workspace swap when the plan (or an explicit config
/// size) asks for more than the game's shipped buffer. Returns the size the
/// workspace will have after the swap, or `0` when nothing was asked for or the
/// swap was refused (fail-open — the game's own buffers stay).
unsafe fn maybe_grow_workspace(
    cfg: &a9_mod::config::Config,
    need: usize,
    entry: &builds::BuildEntry,
    logger: &log::Logger,
) -> usize {
    let target = crate::workspace::target_size(cfg.workspace_out_mb, need, logger);
    if target == 0 {
        return 0;
    }
    let installed = match entry.anchor("workspace_alloc_tail") {
        Some(_) => unsafe { crate::workspace::install(entry, base_ptr(), target, logger.clone()) },
        None => Err("this build has no workspace_alloc_tail anchor".to_string()),
    };
    match installed {
        Ok(line) => {
            logger.line(&line);
            target
        }
        Err(e) => {
            logger.err(&format!(
                "M5-T workspace: swap refused ({e}) — keeping the game's buffer; an oversized \
                 family-A payload still refuses loudly per host"
            ));
            0
        }
    }
}

/// Decide and install the M5 tailgate for this boot's `[[asset]]` ops.
///
/// Merge is the fallback at every step, and every abandonment names its
/// reason in `note` (logged once by the caller):
/// * the whole tailgate is abandoned when the two funnel/cave anchors are
///   absent, a referenced pack's declared size is unreadable, a payload is
///   unreadable, the shipped `i.dat` cannot be read/parsed, or a shipped-name
///   collision lacks `override_vanilla` (the merge refuses that loudly);
/// * per HOST, a payload that does not fit `cap − declared` (or a missing
///   site anchor) falls back to merge for that host alone;
/// * per PACK, the `i.dat` rows fall back to merge when they do not fit or the
///   `i.dat` site anchor is missing.
///
/// Rows and host appends both take their offsets from
/// [`a9_formats::overlay::plan_family_a_lens`], so the two can never disagree.
fn decide_tailgate(
    plan: &crate::overlay::Plan,
    exe_dir: &Path,
    entry: &builds::BuildEntry,
    base: *const u8,
    logger: &log::Logger,
    mode: a9_mod::config::AssetServing,
    cap: usize,
) -> TailgateDecision {
    let no_serve = |declared: BTreeMap<String, usize>, note: String| TailgateDecision {
        declared,
        tailgated: BTreeSet::new(),
        note,
    };
    if plan.assets.is_empty() {
        return TailgateDecision {
            declared: BTreeMap::new(),
            tailgated: BTreeSet::new(),
            note: String::new(),
        };
    }
    // hosts in the resolved op order (first appearance)
    let mut hosts: Vec<String> = Vec::new();
    for a in &plan.assets {
        if !hosts.iter().any(|h| h == &a.host) {
            hosts.push(a.host.clone());
        }
    }
    for l in &plan.listings {
        if !hosts.iter().any(|h| h == &l.host) {
            hosts.push(l.host.clone());
        }
    }
    if mode == a9_mod::config::AssetServing::Merge {
        return no_serve(
            BTreeMap::new(),
            format!(
                "asset serving: merge — asset_serving = \"merge\" ({} asset(s) over {} host \
                 pack(s))",
                plan.assets.len(),
                hosts.len()
            ),
        );
    }

    // 1. vanilla declared sizes — the append base for every host and i.dat
    let read_declared = |key: &str| -> Result<usize, String> {
        let name = if key == "i.dat" {
            "i.dat".to_string()
        } else {
            format!("{key}.bin")
        };
        let p = exe_dir.join("data").join(&name);
        let bytes = std::fs::read(&p).map_err(|e| format!("data\\{name}: {e}"))?;
        a9_formats::pack::family_a_declared_size(&bytes)
            .map_err(|e| format!("data\\{name}: {e}"))
    };
    let mut declared: BTreeMap<String, usize> = BTreeMap::new();
    let mut keys = hosts.clone();
    keys.push("i.dat".to_string());
    for key in &keys {
        match read_declared(key) {
            Ok(d) => {
                declared.insert(key.clone(), d);
            }
            Err(e) => {
                return no_serve(
                    declared,
                    format!("asset serving: merge — tailgate unavailable ({e})"),
                )
            }
        }
    }

    // 2. every payload, read once (the tailgate appends from memory; the rows
    //    need every payload's length, even for hosts that end up merged)
    let mut blobs: Vec<Vec<u8>> = Vec::with_capacity(plan.assets.len());
    for a in &plan.assets {
        match std::fs::read(a.mod_dir.join(&a.file)) {
            Ok(b) => blobs.push(b),
            Err(e) => {
                return no_serve(
                    declared,
                    format!(
                        "asset serving: merge — mod {} asset {} payload unreadable ({e})",
                        a.mod_id, a.name
                    ),
                )
            }
        }
    }

    // 3. the shipped i.dat: the tailgate bypasses merge_idat, so the
    //    authoritative collision check runs here (a collision without
    //    override_vanilla hands the whole thing to merge, which refuses it)
    let shipped = std::fs::read(exe_dir.join("data").join("i.dat"))
        .map_err(|e| format!("reading data\\i.dat: {e}"))
        .and_then(|b| {
            a9_formats::pack::decode_pack(&b, a9_formats::pack::Family::A)
                .map_err(|e| format!("decoding data\\i.dat: {e}"))
        })
        .and_then(|d| a9_formats::idat::parse(&d).map_err(|e| format!("parsing data\\i.dat: {e}")));
    let shipped = match shipped {
        Ok(i) => i,
        Err(e) => {
            return no_serve(
                declared,
                format!("asset serving: merge — tailgate unavailable ({e})"),
            )
        }
    };
    for a in &plan.assets {
        if shipped.row(&a.name).is_some() && !a.override_vanilla {
            return no_serve(
                declared,
                format!(
                    "asset serving: merge — [[asset]] {:?} collides with a shipped i.dat \
                     row without override_vanilla (merge refuses it loudly)",
                    a.name
                ),
            );
        }
    }

    // 4. the funnel + the shared relay cave must be anchored
    let resolve = |name: &str| -> Option<(usize, Vec<u8>)> {
        let a = entry.anchor(name)?;
        let expect = a.expect_bytes().filter(|e| !e.is_empty())?;
        Some((a.rva as usize, expect))
    };
    let (Some((decode_rva, _)), Some((cave_rva, cave_expect))) =
        (resolve("pack_lzss_decode"), resolve("pack_lzss_cave"))
    else {
        return no_serve(
            declared,
            "asset serving: merge — this build lacks the pack_lzss_decode or pack_lzss_cave \
             anchor (or its expect bytes)"
                .into(),
        );
    };

    // 5. per host, the ordered payload list = the host's [[asset]] payloads
    //    (resolved order) then its auto-merged .dir listings — EXACTLY the
    //    order `overlay::host_ops` uses, so the rows and the appends cannot
    //    disagree. The listing payloads were already rebuilt at boot
    //    (plan_listings); the asset payloads are the blobs read in step 2.
    struct Item {
        name: String,
        dir: String,
        payload: Vec<u8>,
    }
    let mut per_host: BTreeMap<String, Vec<Item>> = BTreeMap::new();
    for (i, a) in plan.assets.iter().enumerate() {
        per_host.entry(a.host.clone()).or_default().push(Item {
            name: a.name.clone(),
            dir: a.dir.clone(),
            payload: std::mem::take(&mut blobs[i]),
        });
    }
    for l in &plan.listings {
        per_host.entry(l.host.clone()).or_default().push(Item {
            name: l.name.clone(),
            dir: l.dir.clone(),
            payload: l.payload.clone(),
        });
    }

    // the rows, from the shared planner (offsets == the host appends)
    let mut rows: Vec<a9_formats::idat::NewRow> = Vec::new();
    for host in &hosts {
        let items = &per_host[host];
        let lens: Vec<usize> = items.iter().map(|it| it.payload.len()).collect();
        let layout = a9_formats::overlay::plan_family_a_lens(declared[host], &lens);
        for (it, (off, len)) in items.iter().zip(layout) {
            rows.push(a9_formats::idat::NewRow {
                name: it.name.clone(),
                dir: it.dir.clone(),
                len: len as u32,
                flag: 0,
                off: off as u32,
            });
        }
    }

    // 6. per host: budget-check and build the site plan
    let mut sites: Vec<crate::tailgate::SitePlan> = Vec::new();
    let mut merged: Vec<String> = Vec::new();
    for host in &hosts {
        let items = per_host.get_mut(host).expect("every host has an item list");
        let total: usize = items.iter().map(|it| it.payload.len()).sum();
        let d = declared[host];
        if d + total > cap {
            logger.err(&format!(
                "asset serving: {host} does not fit the decode workspace ({d} declared + \
                 {total} appended > {cap} cap) — merge fallback for this host"
            ));
            merged.push(host.clone());
            continue;
        }
        let anchor_name = tailgate_site_anchor(host);
        let resolved = anchor_name.and_then(|n| {
            let a = entry.anchor(n)?;
            let expect = a.expect_bytes().filter(|e| !e.is_empty())?;
            Some((a.rva as usize, expect))
        });
        let Some((site_rva, site_expect)) = resolved else {
            logger.line(&format!(
                "asset serving: {host}: no tailgate decode site for this build — merge \
                 serving for it"
            ));
            merged.push(host.clone());
            continue;
        };
        let parts: Vec<Vec<u8>> = items.drain(..).map(|it| it.payload).collect();
        sites.push(crate::tailgate::SitePlan {
            key: host.clone(),
            site_rva,
            site_expect,
            declared: d,
            parts,
            idat_rows: 0,
        });
    }

    // 7. the i.dat site: records appended at |V_idat| + the count bump.
    //    The rows are appended at STAGE 1, before the stage-2 host decodes, so
    //    i.dat may only tailgate when EVERY host does: if any host is served
    //    by the merge, the rows must come from the merged index too (its
    //    atomicity refuses the index outright when a host cache fails — a
    //    runtime-appended row whose host then serves vanilla would dangle past
    //    the vanilla image).
    let idat_blob = a9_formats::idat::render_rows(&rows);
    if !merged.is_empty() {
        logger.line(&format!(
            "asset serving: i.dat joins the merge path (host(s) on merge: {})",
            merged.join(", ")
        ));
        merged.push("i.dat".into());
    } else if declared["i.dat"] + idat_blob.len() > TAILGATE_OUT_CAP {
        logger.err(&format!(
            "asset serving: i.dat rows do not fit the decode workspace ({} declared + {} \
             rows > {TAILGATE_OUT_CAP}) — merge fallback for i.dat",
            declared["i.dat"],
            idat_blob.len()
        ));
        merged.push("i.dat".into());
    } else if let Some((site_rva, site_expect)) = resolve("pack_lzss_idat_call") {
        sites.push(crate::tailgate::SitePlan {
            key: "i.dat".into(),
            site_rva,
            site_expect,
            declared: declared["i.dat"],
            parts: vec![idat_blob],
            idat_rows: rows.len() as u32,
        });
    } else {
        logger.line(
            "asset serving: i.dat: no tailgate decode site for this build — merge serving \
             for it",
        );
        merged.push("i.dat".into());
    }

    // 8. install (one cave, every site) — the cave failure is the only fatal one
    let tailgated: BTreeSet<String> = if sites.is_empty() {
        BTreeSet::new()
    } else {
        let install = crate::tailgate::InstallPlan {
            decode_rva,
            cave_rva,
            cave_expect,
            sites,
        };
        match unsafe { crate::tailgate::install(install, base, logger.clone()) } {
            Ok(spliced) => spliced.into_iter().collect(),
            Err(e) => {
                logger.err(&format!(
                    "asset serving: merge — the tailgate relay cave install failed ({e})"
                ));
                BTreeSet::new()
            }
        }
    };

    if !merged.is_empty() {
        logger.line(&format!(
            "asset serving: merge serving for {}",
            merged.join(", ")
        ));
    }
    let note = if tailgated.is_empty() {
        "asset serving: merge — no host pack could be tailgated this boot".to_string()
    } else {
        format!(
            "asset serving: tailgate — {} served in place at the family-A decode ({} asset(s), \
             {} i.dat row(s))",
            tailgated.iter().cloned().collect::<Vec<_>>().join(", "),
            plan.assets.len(),
            rows.len()
        )
    };
    TailgateDecision {
        declared,
        tailgated,
        note,
    }
}
