//! The offline resolution report shared by `a9ml list` and `a9ml doctor`.
//!
//! [`offline_resolution`] does the dry run (read config, scan mods, resolve)
//! and [`print_common`] renders it. Nothing is applied — this is the offline
//! view of what the loader does at boot. The printing is split into one small
//! function per section (identity, config, pack serving, mods header,
//! resolution, and one mod's detail) so a change to a single section stays
//! local; the line order is the report's contract.

use a9_formats as fmt;
use a9_formats::pack::{self, Family};
use a9_mod as amod;
use a9_mod::manifest::CsvMode;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::classify;

/// Everything the offline report needs, gathered once.
pub(crate) struct Offline {
    pub(crate) cfg: amod::config::Config,
    pub(crate) exe: Option<PathBuf>,
    pub(crate) build: Option<amod::buildid::BuildId>,
    pub(crate) scanned: Vec<amod::scan::ScannedMod>,
    pub(crate) manifests: Vec<amod::manifest::Manifest>,
    /// mod dirs, index-aligned with `manifests` (for offline payload checks)
    pub(crate) dirs: Vec<PathBuf>,
    pub(crate) errors: Vec<(PathBuf, String)>,
    pub(crate) resolution: Option<amod::resolve::Resolution>,
}

/// Find the game exe in a game dir (Steam name first, then JP names).
fn find_game_exe(game: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = fs::read_dir(game)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension().map(|x| x == "exe").unwrap_or(false)
                && p.file_name()
                    .map(|n| n.to_string_lossy().starts_with("ATrain9"))
                    .unwrap_or(false)
        })
        .collect();
    candidates.sort_by_key(|p| {
        // prefer ATrain9v5.exe (the international Steam exe), then g_x64
        let n = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        (if n == "ATrain9v5.exe" { 0 } else if n == "ATrain9v5g_x64.exe" { 1 } else { 2 }, n)
    });
    candidates.into_iter().next()
}

/// Read the config, scan the mods dir, resolve the load order. Nothing is
/// applied — this is the dry view of what the loader does at boot.
pub(crate) fn offline_resolution(game: &Path) -> Result<Offline> {
    let cfg = amod::config::read(&game.join("a9loader.toml"));
    let exe = find_game_exe(game);
    let build = exe
        .as_ref()
        .map(|p| fs::read(p).ok())
        .flatten()
        .and_then(|b| amod::buildid::from_exe_bytes(&b));
    let scanned = amod::scan::scan(&game.join(&cfg.mods_dir));
    let mut manifests = Vec::new();
    let mut dirs = Vec::new();
    let mut errors: Vec<(PathBuf, String)> = Vec::new();
    for s in &scanned {
        match &s.manifest {
            Ok(m) => {
                manifests.push(m.clone());
                dirs.push(s.dir.clone());
            }
            Err(e) => errors.push((s.dir.clone(), e.to_string())),
        }
    }
    let resolution = if manifests.is_empty() {
        None
    } else {
        Some(amod::resolve::resolve(&manifests, &cfg.resolve_config()))
    };
    let out = Offline {
        cfg,
        exe,
        build,
        scanned,
        manifests,
        dirs,
        errors,
        resolution,
    };
    Ok(out)
}

/// Best-effort read of the shipped `data/i.dat` row names for the offline
/// `[[asset]]` collision report. `None` = not readable/parseable (the report
/// then says so; the loader's merge-time check stays authoritative).
fn vanilla_asset_names(game: &Path) -> Option<BTreeSet<String>> {
    let file = fs::read(game.join("data/i.dat")).ok()?;
    let decoded = fmt::pack::decode_pack(&file, Family::A).ok()?;
    let idat = fmt::idat::parse(&decoded).ok()?;
    Some(idat.entries.iter().map(|e| e.name.clone()).collect())
}

/// Render the offline resolution: `list` (verbose) and `doctor` share it.
pub(crate) fn print_common(o: &Offline, game: &Path, verbose: bool) {
    print_identity(o, game);
    print_config(o);
    print_pack_serving(o);
    print_mods_header(o, game);
    print_resolution(o, game, verbose);
}

/// Game dir + exe/build identity, and the unknown-build warning.
fn print_identity(o: &Offline, game: &Path) {
    println!("game dir: {}", game.display());
    match (&o.exe, &o.build) {
        (Some(exe), Some(b)) => println!(
            "exe: {} — build id {} (FILEVERSION {})",
            exe.display(),
            b.id,
            b.tuple()
        ),
        (Some(exe), None) => println!("exe: {} — no VERSIONINFO found", exe.display()),
        (None, _) => println!("exe: none found (expected ATrain9v5*.exe)"),
    }
    let book = amod::builds::embedded_book().ok();
    let known_build = o
        .build
        .as_ref()
        .map(|b| book.as_ref().map(|bk| bk.build.contains_key(&b.id)).unwrap_or(false));
    if let Some(false) = known_build {
        println!("  this loader build does NOT know that build id — the loader will apply no memory effects");
    }
}

/// The config line, its warnings, and the probe flag.
fn print_config(o: &Offline) {
    println!(
        "config: mods_dir \"{}\", conflicts={}, disabled {:?}",
        o.cfg.mods_dir.display(),
        if o.cfg.allow_conflicts { "warn+load" } else { "error" },
        o.cfg.disabled
    );
    for w in &o.cfg.warnings {
        println!("  config warning: {w}");
    }
    if o.cfg.probe_fetch {
        println!("  probe: fetch hooks on (diagnostic)");
    }
    if o.cfg.probe_window {
        println!("  probe: window hooks on (diagnostic)");
    }
}

/// How `[[pack]]` effects will be served (merge overlay vs. window injection).
fn print_pack_serving(o: &Offline) {
    match o.cfg.pack_serving {
        amod::config::PackServing::Merge => {
            println!("  packs: merge ([[pack]] effects serve via the M3 cache overlay)")
        }
        amod::config::PackServing::Inject => {
            // the injectable set = family-B image packs ([[pack]] sections);
            // per-pack counts, priority order
            let mut inject_packs = BTreeMap::<String, usize>::new();
            if let Some(res) = &o.resolution {
                for &i in &res.order {
                    for p in &o.manifests[i].pack {
                        if matches!(
                            pack::classify(&p.pack),
                            Some((Family::B, pack::Container::Images))
                        ) {
                            *inject_packs.entry(p.pack.clone()).or_default() += p.entry.len();
                        }
                    }
                }
            }
            if inject_packs.is_empty() {
                println!("  packs: inject (no family-B [[pack]] effects — nothing to serve)");
            } else {
                let joined = inject_packs
                    .iter()
                    .map(|(p, n)| format!("{p} ({n} entr{})", if *n == 1 { "y" } else { "ies" }))
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("  packs: inject — {joined}");
            }
        }
    }
}

/// The mods dir line and the broken-mod list.
fn print_mods_header(o: &Offline, game: &Path) {
    let n_mods = o.scanned.len();
    let dir_word = if n_mods == 1 { "directory" } else { "directories" };
    println!(
        "mods dir: {} — {} {}",
        game.join(&o.cfg.mods_dir).display(),
        n_mods,
        dir_word
    );
    for (dir, e) in &o.errors {
        println!("  broken mod {}: {e}", dir.display());
    }
}

/// Warnings, skips, overlaps, then the resolved order with each mod's detail.
fn print_resolution(o: &Offline, game: &Path, verbose: bool) {
    if let Some(res) = &o.resolution {
        for w in &res.warnings {
            println!("  warning: {w}");
        }
        for s in &res.skipped {
            println!("  skipped '{}': {}", s.id, s.reason);
        }
        for o2 in &res.overlaps {
            let owners: Vec<String> = o2.owners.iter().map(|&i| o.manifests[i].id.clone()).collect();
            println!("  overlap on {}: {} (earlier mod keeps it)", o2.key, owners.join(" vs "));
        }
        let build_id = o.build.as_ref().map(|b| b.id.as_str());
        let vanilla_assets = vanilla_asset_names(game);
        for &i in &res.order {
            print_mod(o, i, build_id, &vanilla_assets, verbose);
        }
        if verbose {
            println!("  (offline resolution — the loader applies this at boot)");
        }
    } else if o.scanned.is_empty() {
        println!("  (no mods — the loader is a pure proxy)");
    }
}

/// One resolved mod: the order line, the classify roll-up, the csv/asset/
/// loose roll-ups, and (verbose) the per-effect detail.
fn print_mod(
    o: &Offline,
    i: usize,
    build_id: Option<&str>,
    vanilla_assets: &Option<BTreeSet<String>>,
    verbose: bool,
) {
    let m = &o.manifests[i];
    let gated = build_id.map(|bid| {
        !m.game_builds.is_empty() && !m.game_builds.iter().any(|b| b == bid)
    });
    let gate_note = match gated {
        Some(true) => "  [GATED: game_builds excludes this build — effects will be refused]",
        _ => "",
    };
    println!(
        "  order: '{}' v{} by {} (prio {}) — {} patch, {} hook{gate_note}",
        m.id,
        m.version,
        m.author,
        *o.cfg.priorities.get(&m.id).unwrap_or(&m.priority),
        m.patch.len(),
        m.hook.len()
    );
    // where the game consumes this mod's data effects (additive line —
    // the `order:`/`packs:` lines above stay byte-compatible)
    let classify_rows = classify::classify_manifest(m);
    if classify_rows.is_empty() {
        if m.loose_asset.is_empty() {
            println!("    classify: (no [[pack]]/[[csv]]/[[asset]]/[[loose_asset]] data effects)");
        }
    } else {
        println!(
            "    classify: {}",
            classify::Summary::of(&classify_rows).compact()
        );
    }
    print_csv_rollup(m);
    // [[asset]] and [[loose_asset]] roll-ups (M5 W1 / W1b, M6); the
    // resolved `host = "auto"` map is also used by the verbose block below
    let auto_hosts = print_asset_rollup(o, i, m, vanilla_assets);
    print_loose_rollup(o, i, m);
    if verbose {
        print_mod_verbose(m, &auto_hosts, &classify_rows);
    }
}

/// csvp data-effect count — row patches distinctly (M5 W2).
fn print_csv_rollup(m: &amod::manifest::Manifest) {
    if !m.csv.is_empty() {
        let n = m.csv.len();
        let rows: usize = m
            .csv
            .iter()
            .filter(|c| c.mode == CsvMode::Rows)
            .map(|c| c.row_keys.len())
            .sum();
        let replaces = m.csv.iter().filter(|c| c.mode == CsvMode::Replace).count();
        print!("    csv: {n} csv table{}", if n == 1 { "" } else { "s" });
        let mut parts: Vec<String> = Vec::new();
        if rows > 0 {
            parts.push(format!(
                "{rows} row patch{}",
                if rows == 1 { "" } else { "es" }
            ));
        }
        if replaces > 0 {
            parts.push(format!(
                "{replaces} whole-table replace{}",
                if replaces == 1 { "" } else { "s" }
            ));
        }
        if parts.is_empty() {
            println!();
        } else {
            println!(" ({})", parts.join(", "));
        }
    }
}

/// The `[[asset]]` roll-up (M5 W1 / W1b): new rows vs overrides of shipped
/// ones, the `host = "auto"` resolution, and the offline payload checks.
/// Returns the resolved auto-host map for the verbose detail.
fn print_asset_rollup(
    o: &Offline,
    i: usize,
    m: &amod::manifest::Manifest,
    vanilla_assets: &Option<BTreeSet<String>>,
) -> BTreeMap<String, Result<String, String>> {
    if m.asset.is_empty() {
        return BTreeMap::new();
    }
    let auto_hosts = print_host_auto(o, m);
    print_asset_collisions(m, vanilla_assets);
    print_asset_payload_check(o, i, m);
    auto_hosts
}

/// Resolve every `host = "auto"` row against the embedded dir index for the
/// running build (the same call the loader makes) and print the roll-up.
fn print_host_auto(
    o: &Offline,
    m: &amod::manifest::Manifest,
) -> BTreeMap<String, Result<String, String>> {
    let mut auto_hosts: BTreeMap<String, Result<String, String>> = BTreeMap::new();
    let autos: Vec<&amod::manifest::AssetSection> = m
        .asset
        .iter()
        .filter(|a| a.host.eq_ignore_ascii_case(amod::manifest::HOST_AUTO))
        .collect();
    if autos.is_empty() {
        return auto_hosts;
    }
    let dirindex = amod::dirindex::embedded_index().ok();
    for a in &autos {
        let r = match (o.build.as_ref(), &dirindex) {
            (Some(b), Some(idx)) => amod::dirindex::resolve_auto(idx, &b.id, &a.name, &a.dir),
            (None, _) => Err(
                "unknown build (no exe identity) — cannot resolve host = \"auto\"".to_string(),
            ),
            (_, None) => Err("the embedded dir index failed to parse".to_string()),
        };
        auto_hosts.insert(a.name.clone(), r);
    }
    let ok = auto_hosts.values().filter(|r| r.is_ok()).count();
    let refused = auto_hosts.len() - ok;
    let mut hosts: Vec<String> = auto_hosts
        .values()
        .filter_map(|r| r.as_ref().ok().cloned())
        .collect();
    hosts.sort();
    hosts.dedup();
    println!(
        "    host auto: {ok} resolved{}; {refused} refused",
        if hosts.is_empty() {
            String::new()
        } else {
            format!(" ({})", hosts.join(", "))
        }
    );
    for (name, r) in &auto_hosts {
        if let Err(e) = r {
            println!("      refusal: {name}: {e}");
        }
    }
    auto_hosts
}

/// New rows vs overrides of shipped i.dat names, and the `override_vanilla`
/// reminder for a colliding name that lacks it.
fn print_asset_collisions(m: &amod::manifest::Manifest, vanilla_assets: &Option<BTreeSet<String>>) {
    let n = m.asset.len();
    match vanilla_assets {
        Some(names) => {
            let over = m.asset.iter().filter(|a| names.contains(&a.name)).count();
            println!(
                "    assets: {n} ({} new, {over} vanilla override{})",
                n - over,
                if over == 1 { "" } else { "s" }
            );
            let need: Vec<&str> = m
                .asset
                .iter()
                .filter(|a| names.contains(&a.name) && !a.override_vanilla)
                .map(|a| a.name.as_str())
                .collect();
            if !need.is_empty() {
                println!(
                    "      warning: {} asset name(s) collide with the shipped i.dat \
                     and need `override_vanilla = true`: {}",
                    need.len(),
                    need.join(", ")
                );
            }
        }
        None => println!(
            "    assets: {n} (vanilla data/i.dat not readable — collisions unchecked \
             offline; the loader refuses them at merge time)"
        ),
    }
}

/// The payload must exist in the mod dir (offline check; the loader also fails
/// loudly at merge time).
fn print_asset_payload_check(o: &Offline, i: usize, m: &amod::manifest::Manifest) {
    let missing: Vec<&str> = m
        .asset
        .iter()
        .filter(|a| o.dirs.get(i).is_some_and(|d| !d.join(&a.file).is_file()))
        .map(|a| a.file.as_str())
        .collect();
    if !missing.is_empty() {
        println!(
            "      warning: {} asset payload file(s) missing in the mod dir: {}",
            missing.len(),
            missing.join(", ")
        );
    }
}

/// The `[[loose_asset]]` roll-up (M6) — game files opened by path, served from
/// a loader cache copy, matched by a case-insensitive glob. The redirect
/// happens at the game's OWN open, so there is no boot-window residency
/// concern (unlike `[[pack]]`/`[[asset]]`).
fn print_loose_rollup(o: &Offline, i: usize, m: &amod::manifest::Manifest) {
    if m.loose_asset.is_empty() {
        return;
    }
    let n = m.loose_asset.len();
    println!(
        "    loose: {n} path redirect{} (served at the game's own open — no \
         boot-window residency)",
        if n == 1 { "" } else { "s" }
    );
    let missing: Vec<&str> = m
        .loose_asset
        .iter()
        .filter(|l| o.dirs.get(i).is_some_and(|d| !d.join(&l.file).is_file()))
        .map(|l| l.file.as_str())
        .collect();
    if !missing.is_empty() {
        println!(
            "      warning: {} loose payload file(s) missing in the mod dir: {} \
             (the loader drops them loudly, fail-open)",
            missing.len(),
            missing.join(", ")
        );
    }
}

/// `--verbose` (`a9ml list`): one line per effect of the mod.
fn print_mod_verbose(
    m: &amod::manifest::Manifest,
    auto_hosts: &BTreeMap<String, Result<String, String>>,
    classify_rows: &[classify::Row],
) {
    for p in &m.patch {
        println!("    [[patch]] {} off={} len={}", p.target.key(p.offset), p.offset, amod::hex::parse(&p.data).map(|d| d.len()).unwrap_or(0));
    }
    for h in &m.hook {
        println!("    [[hook]] {} handler={} list={}", h.target.key(0), h.handler, h.list.as_deref().unwrap_or("-"));
    }
    for p in &m.pack {
        for e in &p.entry {
            println!("    [[pack]] {} entry {} ({}x{}, {})", p.pack, e.name, e.width, e.height, e.file);
        }
    }
    for c in &m.csv {
        println!(
            "    [[csv]] {} ({}, {})",
            c.name,
            c.file,
            match c.mode {
                CsvMode::Replace => "replace",
                CsvMode::Rows => "rows",
            }
        );
    }
    for l in &m.loose_asset {
        println!("    [[loose_asset]] match \"{}\" <- {}", l.match_, l.file);
    }
    for a in &m.asset {
        let host = match auto_hosts.get(&a.name) {
            Some(Ok(h)) => format!("{h} [auto]"),
            Some(Err(_)) => "auto [REFUSED]".to_string(),
            None => a.host.clone(),
        };
        println!(
            "    [[asset]] {} (host {host}, dir {}, file {}{})",
            a.name,
            a.dir,
            a.file,
            if a.override_vanilla { ", override_vanilla" } else { "" }
        );
    }
    for r in classify_rows {
        println!("      {}", r.line());
    }
}
