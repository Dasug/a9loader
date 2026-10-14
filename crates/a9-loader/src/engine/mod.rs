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
//!
//! [`boot`] is the narrative: each phase is a named step —
//! [`build_gate`], [`scan_and_resolve`], [`apply_memory_effects`], the
//! [`PlanBuilder`] data plan, [`install_inject`], [`install_overlay`],
//! [`install_probe`] — and the tailgate/listing planning lives in
//! [`tailgate_plan`], the journal + final count line in [`report`].

mod plan;
mod report;
mod tailgate_plan;

use crate::{forms, handlers, log, sys};
use a9_mod::{buildid, builds, config, manifest::Manifest, resolve, scan};
use plan::PlanBuilder;
use std::collections::BTreeSet;
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
    logger.line(&config_line(&cfg, &exe_dir));

    // ---- the loader-off escape hatch: enabled = false boots fully vanilla
    // (the proxy stays, everything else is skipped — no scan, no effects,
    // no hooks). The escape hatch reads the SAME toml; flipping is a one-line
    // edit or a `--disable-...`-style wrapper for Steam launch options later.
    if !cfg.enabled {
        logger.line("loader disabled by config (enabled = false) — pure proxy passthrough");
        return;
    }

    // ---- build identity (from the exe file; no version.dll calls in DllMain)
    let Some((build_id, entry)) = build_gate(&exe, &logger) else {
        return;
    };
    let base = unsafe { base_ptr() };

    // ---- M7-0: the save-id clamp guard. ALWAYS ON (a safety fix, not a
    // diagnostic): a saved formation's form id is used as a spec-row index
    // with no bounds check, so an out-of-range id reads past the heap block.
    // Installed before the mod scan so it runs even with no mods at all —
    // a save made with a mod that is now absent must still degrade to the
    // engine's invalid-row state instead of crashing. Fail-open: a missing
    // anchor / expect mismatch refuses loudly and the game boots vanilla.
    match unsafe {
        crate::clamp::install(
            &entry,
            base,
            logger.clone(),
            cfg.log_level >= a9_mod::config::LogLevel::Debug,
        )
    } {
        Ok(line) => logger.line(&line),
        Err(e) => logger.err(&format!(
            "save clamp: {e} — out-of-range save ids are NOT guarded this boot (fail-open)"
        )),
    }

    // ---- scan + resolve (disabled ids are excluded with one line each —
    // a disabled mod is user intent, not an error)
    let Some((manifests, dirs, res)) = scan_and_resolve(&mods_dir, &cfg, &entry, &logger) else {
        return;
    };

    // ---- execute
    let (applied, refused) =
        unsafe { apply_memory_effects(&manifests, &dirs, &res, &build_id, &entry, base, &logger) };

    // ---- data effects (M3/M4): [[pack]]/[[csv]] serving + [[file]] boot copies
    // pack_serving == "inject" routes family-B [[pack]] effects through the
    // M4 window-scoped injector instead of the merge overlay; [[csv]] keeps
    // the M3 merge mechanism in both modes; [[file]] copies are unaffected.
    let inject_mode = cfg.pack_serving == a9_mod::config::PackServing::Inject;
    let mut builder = PlanBuilder::new(&res, &build_id, inject_mode, &logger);
    for &i in &res.order {
        builder.add_mod(i, &manifests[i], &dirs[i]);
    }
    let (mut plan, mut inject_plan, file_copies, refused_data) = builder.finish();
    let refused = refused + refused_data;

    // [[file]] copies — genuinely-new files, before the game enumerates
    // anything (we run before main); idempotent, never overwrites
    for (mod_id, f, mod_dir) in &file_copies {
        report::copy_file_effect(mod_id, f, mod_dir, &exe_dir, &mods_dir, &logger);
    }

    note_asset_serving(&cfg, &plan, &logger);

    // ---- M5-U: derive the auto-merged `.dir` group listings from the mods'
    // OWN [[asset]] rows (an author never writes a `.dir` file). Runs before
    // the serving decision: the rebuilt listing payload rides its host pack
    // exactly like an asset payload and its row joins the i.dat rows on
    // whichever path serves that host.
    plan.listings = tailgate_plan::plan_listings(&plan, &exe_dir, &logger);

    // ---- M5 tailgate: [[asset]] new assets appended in place at the family-A
    // decode (no cache, no re-encode). Decided per host at boot; merge is the
    // fallback for anything that does not line up. Runs BEFORE the overlay
    // install — the overlay must know which packs to leave alone.
    unsafe { tailgate_plan::decide_and_grow(&mut plan, &exe_dir, &cfg, &entry, base, &logger) };

    // ---- the M4 inject splice (before the overlay hook — a failed splice
    // must fall back to merge BEFORE the overlay plan is finalized). The
    // splice is the ONLY thing inject mode installs: it rewrites the
    // expect-verified family-B runner call over to the injector stub; the
    // stub itself runs per pack window at runtime. Any failure is fail-open:
    // the packs return to the merge plan (today's behavior) untouched.
    let injector_live = unsafe {
        install_inject(&mut plan, &mut inject_plan, &cfg, &entry, base, &mods_dir, &logger)
    };

    // the overlay hook — one CFile::Open hook serves every matched pack; in
    // inject mode (with the plan moved out) it doubles as the injector's
    // pack-identity capture seam
    install_overlay(plan, &cfg, &exe_dir, &mods_dir, &entry, &logger, injector_live);

    // ---- M4 fetch probe (opt-in diagnostics; trampoline-only, fail-open)
    // Installs after the build gate + overlay block; each anchor
    // expect-verifies like every other write. Pure diagnostics: nothing
    // downstream changes behavior, all failure paths log loudly and the
    // game boots vanilla-booted.
    if cfg.probe_fetch {
        install_probe(&entry, base, &mods_dir, &logger);
    }

    // ---- M7 purchase-window probe (opt-in diagnostics; trampoline-only,
    // fail-open): dumps the spec-table manager's three index vectors on the
    // window's entry/exit whenever a size changes — the observable of the
    // notes/23 type-cycle quirk. No behaviour change.
    if cfg.probe_window {
        install_window_probe(&entry, base, &mods_dir, &logger);
    }

    // ---- M7-2 save-path probe (opt-in diagnostics; trampoline-only,
    // fail-open): hooks the four save-side mmf helpers and logs the caller RVA
    // + size of every save-path write, so one in-game save answers whether a
    // formation's car block is one bulk 0xf8 write (the 20-byte tails then
    // round-trip verbatim) or ten 0x18 writes (the tails are regenerated), and
    // which caller RVA is the group-record writer. No behaviour change.
    if cfg.probe_save {
        install_save_probe(&entry, base, &mods_dir, &logger);
    }

    logger.line(&report::result_line(applied, refused, file_copies.len()));
}

/// The `config:` log line (mods dir, conflicts mode, log level, serving modes).
fn config_line(cfg: &config::Config, exe_dir: &Path) -> String {
    format!(
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
    )
}

/// Build identity gate: read the exe FILE (no version.dll APIs in DllMain),
/// fingerprint it, and resolve the embedded address-book entry. `None` = stop
/// the boot (each reason logged). Returns the running build id + its anchors.
fn build_gate(exe: &Path, logger: &log::Logger) -> Option<(String, builds::BuildEntry)> {
    let Ok(exe_bytes) = std::fs::read(exe) else {
        logger.err("cannot read the exe image for build identification — no memory effects");
        return None;
    };
    let Some(build) = buildid::from_exe_bytes(&exe_bytes) else {
        logger.err("exe has no VERSIONINFO — no memory effects (vanilla boot)");
        return None;
    };
    logger.line(&format!(
        "exe build id {} (FILEVERSION {})",
        build.id,
        build.tuple()
    ));
    let Ok(book) = builds::embedded_book() else {
        logger.err("the embedded address book failed to parse — no memory effects");
        return None;
    };
    let Some(entry) = book.build.get(&build.id) else {
        logger.err(&format!(
            "unknown build {} — no memory effects (vanilla boot); this loader supports: {}",
            build.id,
            book.build.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
        return None;
    };
    Some((build.id.clone(), entry.clone()))
}

/// Scan `mods/`, drop disabled ids with one line each, and resolve the
/// survivors (deps/conflicts/overlaps). `None` = no mods (or none left) —
/// the boot continues as a pure proxy, after honoring an explicit
/// `workspace_out_mb` (M5-T is config-driven).
fn scan_and_resolve(
    mods_dir: &Path,
    cfg: &config::Config,
    entry: &builds::BuildEntry,
    logger: &log::Logger,
) -> Option<(Vec<Manifest>, Vec<PathBuf>, resolve::Resolution)> {
    let scanned = scan::scan(mods_dir);

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
                    "mod: {} v{} by {} ({} patch, {} hook, {} pack entr{}, {} csv, {} asset, {} file, {} loose)",
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
                    m.file.len(),
                    m.loose_asset.len()
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
            let _ = tailgate_plan::maybe_grow_workspace(cfg, 0, entry, logger);
        }
        return None;
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
    Some((manifests, dirs, res))
}

/// Apply every mod's `[[patch]]`/`[[hook]]` in resolved order and compose the
/// `train_list` contributions into ONE filter install (M5 W3 — several mods
/// may each declare forms to offer/hide; a priority tie must not decide
/// membership). Returns `(applied, refused)`.
unsafe fn apply_memory_effects(
    manifests: &[Manifest],
    dirs: &[PathBuf],
    res: &resolve::Resolution,
    build_id: &str,
    entry: &builds::BuildEntry,
    base: *const u8,
    logger: &log::Logger,
) -> (usize, usize) {
    let mut applied = 0usize;
    let mut refused = 0usize;
    // `train_list` contributions are collected here and composed into ONE
    // filter install after the loop (M5 W3 — several mods may each declare
    // forms to offer/hide; a priority tie must not decide membership).
    let mut train_list: Vec<handlers::Contribution> = Vec::new();
    for &i in &res.order {
        let (a, r, c) = unsafe {
            apply_mod_effects(
                i,
                &manifests[i],
                &dirs[i],
                res,
                build_id,
                entry,
                base,
                logger,
            )
        };
        applied += a;
        refused += r;
        if let Some(c) = c {
            train_list.push(c);
        }
    }
    // the composed train_list filter — one install for every contribution
    if !train_list.is_empty() {
        let (a, r) = unsafe { install_composed_train_list(&train_list, entry, base, logger) };
        applied += a;
        refused += r;
    }
    (applied, refused)
}

/// Apply ONE mod's `[[patch]]`/`[[hook]]` effects (its `game_builds` gate
/// first). Returns `(applied, refused, train_list contribution)` — the
/// contribution, if any, is composed by the caller into the single filter.
#[allow(clippy::too_many_arguments)]
unsafe fn apply_mod_effects(
    i: usize,
    m: &Manifest,
    dir: &Path,
    res: &resolve::Resolution,
    build_id: &str,
    entry: &builds::BuildEntry,
    base: *const u8,
    logger: &log::Logger,
) -> (usize, usize, Option<handlers::Contribution>) {
    let mut applied = 0usize;
    let mut refused = 0usize;
    let mut contribution = None;
    if !m.game_builds.is_empty() && !m.game_builds.iter().any(|b| b == build_id) {
        logger.err(&format!(
            "mod '{}': game_builds {:?} excludes running build {} — memory effects refused",
            m.id, m.game_builds, build_id
        ));
        refused += m.patch.len() + m.hook.len();
        return (applied, refused, contribution);
    }
    for p in &m.patch {
        let key = format!("patch:{}", p.target.key(p.offset));
        if res.effect_refused(i, &key) {
            logger.err(&format!("mod '{}': {key} refused (overlap loser)", m.id));
            refused += 1;
            continue;
        }
        match unsafe { handlers::apply_patch(p, entry, base) } {
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
            match unsafe { handlers::train_list_contribution(h, entry, base, dir, &m.id) } {
                Ok(c) => contribution = Some(c),
                Err(e) => {
                    logger.err(&format!("mod '{}': {key} refused: {e}", m.id));
                    refused += 1;
                }
            }
            continue;
        }
        match unsafe { handlers::apply_hook(h, entry, base, dir) } {
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
    (applied, refused, contribution)
}

/// Install the ONE composed `train_list` filter: read the FORM list length for
/// the log, install, and report the outcome. Returns `(applied, refused)`.
///
/// On a successful install this also arms the M7 type-tab rebuild fix
/// (`crate::tabrebuild`): the purchase window rebuilds its list on every type
/// tab (re)build through a function that hard-codes the vanilla whitelist, so
/// without the fix every composed row that is not in the vanilla 97 vanishes on
/// the first cycle (notes/23). A failed fix install is fail-open — the filter
/// still works, only the cycle quirk remains — and is logged loudly.
unsafe fn install_composed_train_list(
    train_list: &[handlers::Contribution],
    entry: &builds::BuildEntry,
    base: *const u8,
    logger: &log::Logger,
) -> (usize, usize) {
    let forms_len = unsafe {
        entry
            .anchor("form_list")
            .and_then(|a| handlers::read_u16s_bounded(base, a.rva as usize, 16384).ok())
            .map(|u| forms::parse_form_list(&u).len())
            .unwrap_or(417)
    };
    match unsafe { handlers::install_train_list(train_list, entry, base, forms_len, logger) } {
        Ok(journal) => {
            logger.line(&journal);
            // M7: the type-tab cycle fix (only meaningful with a composed
            // filter installed — that is exactly this branch)
            match unsafe { crate::tabrebuild::install(entry, base) } {
                Ok(line) => logger.line(&line),
                Err(e) => logger.err(&format!(
                    "tab cycle fix: {e} — the purchase-list filter cycle keeps \
                     its vanilla behaviour (fail-open)"
                )),
            }
            // M7: the custom-train-form editor's candidate list rebuilds
            // through a second hard-coded vanilla-whitelist scan (notes/23);
            // the same composed keep-set gates it. Fail-open like the tab fix.
            match unsafe { crate::formreadout::install(entry, base) } {
                Ok(line) => logger.line(&line),
                Err(e) => logger.err(&format!(
                    "form readout fix: {e} — the custom-train editor keeps its \
                     vanilla behaviour (fail-open)"
                )),
            }
            (1, 0)
        }
        Err(e) => {
            logger.err(&format!(
                "train_list: {e} — {} contribution(s) refused",
                train_list.len()
            ));
            (0, train_list.len())
        }
    }
}

/// `[[asset]]` serving is orthogonal to pack_serving: the tailgate (the
/// default) has a family-A seam of its own, so inject mode does not force
/// these onto the merge path. Say which path applies when the two are set
/// to different mechanisms so a user reading the log is not surprised.
fn note_asset_serving(cfg: &config::Config, plan: &crate::overlay::Plan, logger: &log::Logger) {
    let inject_mode = cfg.pack_serving == a9_mod::config::PackServing::Inject;
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
}

/// The M4 inject splice: verify the famb site + relay cave anchors and install
/// the stub; on any refusal move the inject ops back into the merge plan
/// (fail-open — the packs serve exactly as they would in merge mode). Returns
/// whether the injector is live.
unsafe fn install_inject(
    plan: &mut crate::overlay::Plan,
    inject_plan: &mut crate::injector::InjectorPlan,
    cfg: &config::Config,
    entry: &builds::BuildEntry,
    base: *const u8,
    mods_dir: &Path,
    logger: &log::Logger,
) -> bool {
    let inject_mode = cfg.pack_serving == a9_mod::config::PackServing::Inject;
    if !(inject_mode && !inject_plan.is_empty()) {
        return false;
    }
    let site_anchor = entry.anchor("famb_runner_call");
    let cave_anchor = entry.anchor("famb_runner_cave");
    match (site_anchor, cave_anchor) {
        (Some(anchor), Some(cave_anchor)) => match (anchor.expect_bytes(), cave_anchor.expect_bytes())
        {
            (Some(expect), Some(cave_expect)) if !expect.is_empty() && !cave_expect.is_empty() => {
                match unsafe {
                    crate::injector::install(
                        inject_plan,
                        base,
                        anchor.rva as usize,
                        &expect,
                        cave_anchor.rva as usize,
                        &cave_expect,
                        log::Logger::new(&mods_dir.join("a9loader.log")),
                    )
                } {
                    Ok(()) => true,
                    Err(e) => {
                        logger.err(&format!(
                            "inject refused: famb_runner_call rva={:#x}: {e} — \
                             [[pack]] effects fall back to merge serving",
                            anchor.rva
                        ));
                        for (pack, ops) in std::mem::take(&mut inject_plan.packs) {
                            plan.images.entry(pack).or_default().extend(ops);
                        }
                        false
                    }
                }
            }
            _ => {
                logger.err(
                    "inject refused: famb anchors have no expect bytes in the \
                     address book — [[pack]] effects fall back to merge",
                );
                false
            }
        },
        _ => {
            logger.err(
                "inject refused: this build lacks the famb_runner_call or \
                 famb_runner_cave anchor (older address book?) — [[pack]] \
                 effects fall back to merge",
            );
            false
        }
    }
}

/// Install the overlay hook (a no-op when there is nothing to serve). With the
/// `cfile_open` anchor present, [`install_overlay_hooked`] does the work; a
/// missing anchor is a loud fail-open that still names every loose asset.
fn install_overlay(
    plan: crate::overlay::Plan,
    cfg: &config::Config,
    exe_dir: &Path,
    mods_dir: &Path,
    entry: &builds::BuildEntry,
    logger: &log::Logger,
    injector_live: bool,
) {
    if plan.images.is_empty()
        && plan.csvs.is_empty()
        && plan.assets.is_empty()
        && plan.listings.is_empty()
        && plan.loose.is_empty()
        && !injector_live
    {
        return;
    }
    match entry.anchor("cfile_open") {
        Some(anchor) => install_overlay_hooked(
            plan, cfg, exe_dir, mods_dir, anchor, logger, injector_live,
        ),
        None => {
            logger.err("overlay refused: this build has no cfile_open anchor");
            if injector_live {
                logger.err(
                    "inject serving cannot fire without the capture hook — \
                     [[pack]] inject effects serve vanilla this boot",
                );
            }
            for l in &plan.loose {
                logger.err(&format!(
                    "loose: mod {} pattern \"{}\": this build has no cfile_open \
                     anchor — this loose asset serves the original file this boot \
                     (fail-open)",
                    l.mod_id, l.pattern
                ));
            }
        }
    }
}

/// The counts and log fragments for the `overlay: cfile_open hooked` summary.
struct OverlaySummary {
    image_count: usize,
    csv_count: usize,
    asset_count: usize,
    listing_count: usize,
    loose_count: usize,
    packs: Vec<String>,
    asset_note: String,
    loose_note: String,
}

impl OverlaySummary {
    /// Fold the plan into the summary the hook-install line reports: the entry
    /// count, distinct csv tables, asset/listing/loose counts, the touched
    /// pack list, and the asset/loose log fragments.
    fn of(plan: &crate::overlay::Plan) -> OverlaySummary {
        let image_count: usize = plan.images.values().map(|v| v.len()).sum();
        let csv_count = {
            let mut v: Vec<&str> = plan.csvs.iter().map(|c| c.name.as_str()).collect();
            v.sort_unstable();
            v.dedup();
            v.len()
        };
        let asset_count = plan.assets.len();
        let listing_count = plan.listings.len();
        let loose_count = plan.loose.len();
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
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect()
            })
            .collect();
        let asset_note = if asset_count > 0 {
            let listing = if listing_count > 0 {
                format!(
                    ", {listing_count} extended listing{}",
                    if listing_count == 1 { "" } else { "s" }
                )
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
        let loose_note = if loose_count > 0 {
            format!(
                "; {loose_count} loose asset{}",
                if loose_count == 1 { "" } else { "s" }
            )
        } else {
            String::new()
        };
        OverlaySummary {
            image_count,
            csv_count,
            asset_count,
            listing_count,
            loose_count,
            packs,
            asset_note,
            loose_note,
        }
    }
}

/// Install the `cfile_open` hook with the `cfile_open` anchor in hand and log
/// the summary (or the loud fail-open naming every loose asset).
fn install_overlay_hooked(
    plan: crate::overlay::Plan,
    cfg: &config::Config,
    exe_dir: &Path,
    mods_dir: &Path,
    anchor: &builds::Anchor,
    logger: &log::Logger,
    injector_live: bool,
) {
    let OverlaySummary {
        image_count,
        csv_count,
        asset_count,
        listing_count,
        loose_count,
        packs,
        asset_note,
        loose_note,
    } = OverlaySummary::of(&plan);
    // a failed hook install must still name every loose asset it
    // could not serve (fail-open: the original open proceeds)
    let loose_summary: Vec<(String, String)> = plan
        .loose
        .iter()
        .map(|l| (l.mod_id.clone(), l.pattern.clone()))
        .collect();
    match unsafe {
        crate::overlay::install(
            plan,
            cfg.mods_dir.clone(),
            exe_dir.to_path_buf(),
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
                && listing_count == 0
                && loose_count == 0 =>
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
             across packs: {}; {csv_count} csv table{}{asset_note}{loose_note})",
            anchor.rva,
            if image_count == 1 { "y" } else { "ies" },
            packs.join(", "),
            if csv_count == 1 { "" } else { "s" },
        )),
        Err(e) => report_overlay_failure(anchor.rva, injector_live, &loose_summary, &e, logger),
    }
}

/// The overlay hook-install failure report: the refusal line, the inject
/// warning when the capture hook is load-bearing, and one line naming every
/// loose asset that cannot be served this boot (fail-open: the original open
/// proceeds).
fn report_overlay_failure(
    anchor_rva: u32,
    injector_live: bool,
    loose_summary: &[(String, String)],
    e: &str,
    logger: &log::Logger,
) {
    logger.err(&format!("overlay refused: cfile_open rva={:#x}: {e}", anchor_rva));
    if injector_live {
        logger.err(
            "inject serving cannot fire without the capture \
             hook — [[pack]] inject effects serve vanilla \
             this boot",
        );
    }
    for (id, pat) in loose_summary {
        logger.err(&format!(
            "loose: mod {id} pattern \"{pat}\": the overlay hook \
             could not be installed — this loose asset serves the \
             original file this boot (fail-open)"
        ));
    }
}

/// The M4 fetch probe (opt-in diagnostics): collect the probe specs from the
/// address book and install the trampolines; a failure logs loudly and leaves
/// the rest of the boot untouched.
fn install_probe(
    entry: &builds::BuildEntry,
    base: *const u8,
    mods_dir: &Path,
    logger: &log::Logger,
) {
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
    if let Err(e) = unsafe {
        crate::probe::install(
            log::Logger::new(&mods_dir.join("a9loader.log")),
            base,
            &specs,
        )
    } {
        logger.err(&format!("probe: {e} — remaining boot unaffected"));
    }
}

/// The M7 purchase-window probe (opt-in diagnostics): hook the type-tab
/// rebuild entry and the big list window entry trampoline-only and dump the
/// spec-table manager's index vectors on entry/exit whenever a size changes
/// (notes/23). A failure logs loudly and leaves the boot vanilla.
fn install_window_probe(
    entry: &builds::BuildEntry,
    base: *const u8,
    mods_dir: &Path,
    logger: &log::Logger,
) {
    logger.line("probe: window hooks on (diagnostic)");
    match unsafe {
        crate::probe_window::install(
            log::Logger::new(&mods_dir.join("a9loader.log")),
            base,
            entry,
        )
    } {
        Ok(lines) => {
            for l in lines {
                logger.line(&l);
            }
        }
        Err(e) => logger.err(&format!(
            "probe refused: window: {e} — remaining boot unaffected"
        )),
    }
}

/// The M7-2 save-path probe (opt-in diagnostics): hook the four save-side mmf
/// helpers trampoline-only and log the caller RVA + size + first bytes of every
/// save-path write (notes/62-B §4.2). A failure logs loudly and leaves the boot
/// vanilla.
fn install_save_probe(
    entry: &builds::BuildEntry,
    base: *const u8,
    mods_dir: &Path,
    logger: &log::Logger,
) {
    logger.line("probe: save hooks on (diagnostic)");
    match unsafe {
        crate::probe_save::install(
            log::Logger::new(&mods_dir.join("a9loader.log")),
            base,
            entry,
        )
    } {
        Ok(lines) => {
            for l in lines {
                logger.line(&l);
            }
        }
        Err(e) => logger.err(&format!(
            "probe refused: save: {e} — remaining boot unaffected"
        )),
    }
}

/// The game exe's mapped image base (GetModuleHandle(NULL) — ASLR-safe with
/// book RVAs).
unsafe fn base_ptr() -> *const u8 {
    unsafe { sys::GetModuleHandleW(std::ptr::null()) as *const u8 }
}
