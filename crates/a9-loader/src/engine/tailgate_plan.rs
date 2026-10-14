//! The M5 boot-time tailgate planning and the M5-U `.dir` listing planner.
//!
//! [`plan_listings`] derives the auto-merged `.dir` group listings from the
//! mods' own `[[asset]]` rows; [`decide_and_grow`] runs the tailgate decision
//! (once against the workspace the game will allocate, then again against a
//! grown workspace when the M5-T swap is armed) and records the outcome on the
//! plan. Both are pure boot-time planning: they read files, log loudly, and
//! never change the running game except through the tailgate install.

use crate::log;
use crate::overlay::Plan;
use a9_mod::{builds, config};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The engine's decode-workspace ceiling (`DAT_144f86098` out-cap = 0xC800000,
/// 200 MiB): appended bytes must fit under it with the vanilla image.
const TAILGATE_OUT_CAP: usize = 0xC800000;

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
pub(super) fn plan_listings(
    plan: &Plan,
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
    let shipped = match read_shipped_idat(exe_dir) {
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
        if let Some(op) = plan_one_listing(dir, plan, &shipped, &mut host_cache, exe_dir, logger) {
            out.push(op);
        }
    }
    out
}

/// Plan ONE group's `.dir` listing (`dir` is the group key; the listing row is
/// `<dir>.dir`). Returns the rebuilt op, or `None` when the group is skipped
/// for any of the documented reasons (already logged).
fn plan_one_listing(
    dir: &str,
    plan: &Plan,
    shipped: &a9_formats::idat::Idat,
    host_cache: &mut BTreeMap<String, Result<Vec<u8>, String>>,
    exe_dir: &Path,
    logger: &log::Logger,
) -> Option<crate::overlay::ListingOpPlan> {
    let lname = format!("{dir}.dir");
    let row = shipped.row(&lname)?; // not a listing group — the assets just add normally
    let (contributors, mods, host) = listing_group(plan, dir, &lname, logger)?;
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
            return None;
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
        return None;
    };
    let names: Vec<String> = contributors.iter().map(|a| a.name.clone()).collect();
    match a9_formats::overlay::listing_extend(vanilla, &names) {
        Ok((payload, added, total)) => {
            if added == 0 {
                logger.debug(&format!(
                    "overlay listing: {lname}: every contributed name is already listed — \
                     no extension"
                ));
                return None;
            }
            logger.line(&format!(
                "overlay listing: {lname} extended by {added} name(s) -> {} entries \
                 (host {host}; mod(s) {})",
                thousands(total),
                join_list(&mods)
            ));
            Some(crate::overlay::ListingOpPlan {
                host,
                name: lname,
                dir: row.dir.clone(),
                payload,
            })
        }
        Err(e) => {
            logger.err(&format!(
                "overlay listing: {lname} auto-merge refused: {e} — the shipped listing stays \
                 vanilla (ship an explicit [[asset]] override of the row if you need it)"
            ));
            None
        }
    }
}

/// The membership + host checks for one group's listing (M5-U): the group's
/// contributors, its mods, and the one host its assets resolved to. `None` =
/// the group is skipped for a documented reason (already logged): not on the
/// auto-merge allow-list, an explicit `[[asset]]` override of the row is
/// present, or its assets resolved to different hosts.
fn listing_group<'p>(
    plan: &'p Plan,
    dir: &str,
    lname: &str,
    logger: &log::Logger,
) -> Option<(Vec<&'p crate::overlay::AssetOpPlan>, Vec<String>, String)> {
    let contributors: Vec<&crate::overlay::AssetOpPlan> =
        plan.assets.iter().filter(|a| &a.dir == dir).collect();
    let mut mods: Vec<String> = Vec::new();
    for a in &contributors {
        if !mods.iter().any(|m| m == &a.mod_id) {
            mods.push(a.mod_id.clone());
        }
    }

    if !a9_formats::overlay::listing_auto_merge(lname) {
        logger.line(&format!(
            "overlay listing: {lname} NOT auto-merged ({} name(s) from mod(s) {}) — this \
             listing's consumer may be positional, so the loader will not compose it; ship \
             an explicit [[asset]] name = \"{lname}\" override_vanilla = true to extend it",
            contributors.len(),
            join_list(&mods)
        ));
        return None;
    }
    // an explicit override of the listing row keeps that author in control:
    // serve it byte-for-byte rather than deriving on top of it
    if plan.assets.iter().any(|a| a.name == lname) {
        logger.line(&format!(
            "overlay listing: {lname} not auto-merged (an explicit [[asset]] override of \
             the row is present) — the override is served byte-for-byte"
        ));
        return None;
    }
    // the group's host must be unambiguous (all its assets resolve to one)
    let host = contributors[0].host.clone();
    if contributors.iter().any(|a| a.host != host) {
        logger.err(&format!(
            "overlay listing: {lname} not auto-merged (its group's assets resolved to \
             different host packs) — ship an explicit [[asset]] override of the row"
        ));
        return None;
    }
    Some((contributors, mods, host))
}

/// Read + decode + parse the shipped `data\i.dat` (the authoritative row
/// table). The error string names the failing step.
fn read_shipped_idat(exe_dir: &Path) -> Result<a9_formats::idat::Idat, String> {
    std::fs::read(exe_dir.join("data").join("i.dat"))
        .map_err(|e| format!("reading data\\i.dat: {e}"))
        .and_then(|b| {
            a9_formats::pack::decode_pack(&b, a9_formats::pack::Family::A)
                .map_err(|e| format!("decoding data\\i.dat: {e}"))
        })
        .and_then(|d| a9_formats::idat::parse(&d).map_err(|e| format!("parsing data\\i.dat: {e}")))
}

// ---- M5 tailgate boot decision -------------------------------------------------

struct TailgateDecision {
    /// per-host vanilla declared size (the append base == the i.dat row offset)
    declared: BTreeMap<String, usize>,
    /// packs (incl. "i.dat") the tailgate actually serves; the overlay must
    /// leave exactly these alone (no redirect, no cache)
    tailgated: BTreeSet<String>,
    /// the one loud line describing the chosen path (or empty)
    note: String,
}

/// A per-host append item — an `[[asset]]` payload or an auto-merged `.dir`
/// listing, in the order [`crate::overlay`]'s `host_ops` uses.
struct Item {
    name: String,
    dir: String,
    payload: Vec<u8>,
}

/// The abandonment of the tailgate plan: the declared sizes read so far and the
/// one loud line naming the reason (the caller turns it into a merge decision).
struct Abandon {
    declared: BTreeMap<String, usize>,
    note: String,
}

fn no_serve(declared: BTreeMap<String, usize>, note: String) -> TailgateDecision {
    TailgateDecision {
        declared,
        tailgated: BTreeSet::new(),
        note,
    }
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

/// The hosts a tailgate decision touches, in first-appearance order: every
/// `[[asset]]` host, then every listing host that is not already present.
fn tailgate_hosts(plan: &Plan) -> Vec<String> {
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
    hosts
}

/// The largest family-A host requirement this plan implies: the host's vanilla
/// declared size plus every payload the loader appends to it (`[[asset]]`
/// payloads and the M5-U derived `.dir` listings). `declared` is the map the
/// tailgate decision read at boot, so this is exact whenever that read worked.
fn plan_family_a_need(plan: &Plan, declared: &BTreeMap<String, usize>) -> usize {
    let mut need: BTreeMap<String, usize> = declared.clone();
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
pub(super) unsafe fn maybe_grow_workspace(
    cfg: &config::Config,
    need: usize,
    entry: &builds::BuildEntry,
    logger: &log::Logger,
) -> usize {
    let target = crate::workspace::target_size(cfg.workspace_out_mb, need, logger);
    if target == 0 {
        return 0;
    }
    let installed = match entry.anchor("workspace_alloc_tail") {
        Some(_) => unsafe {
            crate::workspace::install(entry, crate::engine::base_ptr(), target, logger.clone())
        },
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

/// Decide and install the M5 tailgate for this boot's `[[asset]]` ops: first
/// against the workspace the game will allocate, then — when the M5-T swap is
/// armed — against the grown cap, and finally record the outcome on `plan`.
///
/// The two-pass shape is load-bearing: the game allocates the decode workspace
/// once at boot and bounds the LZSS decoder by its OUT cap, so a mod set whose
/// family-A content does not fit is refused per host by the tailgate's own
/// budget check. When the plan needs more, [`maybe_grow_workspace`] grows the
/// workspace at the game's alloc tail — before any decode, cap raised, buffer
/// from the game's own `_aligned_malloc` — and the decision re-runs against the
/// grown cap (otherwise an oversized host would "fall back" to a merge path
/// whose 150 MB encoded-file buffer cannot serve it either). AUTO by default;
/// `workspace_out_mb` forces a size. Fail-open: a refusal keeps the game's
/// buffers and says so.
pub(super) unsafe fn decide_and_grow(
    plan: &mut Plan,
    exe_dir: &Path,
    cfg: &config::Config,
    entry: &builds::BuildEntry,
    base: *const u8,
    logger: &log::Logger,
) {
    let mut decision = decide_tailgate(
        plan,
        exe_dir,
        entry,
        base,
        logger,
        cfg.asset_serving,
        TAILGATE_OUT_CAP,
    );
    let needed = plan_family_a_need(plan, &decision.declared);
    let grown = unsafe { maybe_grow_workspace(cfg, needed, entry, logger) };
    if grown > 0 {
        decision = decide_tailgate(plan, exe_dir, entry, base, logger, cfg.asset_serving, grown);
    }
    plan.declared = decision.declared;
    plan.tailgated = decision.tailgated;
    if !decision.note.is_empty() {
        logger.line(&decision.note);
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
    plan: &Plan,
    exe_dir: &Path,
    entry: &builds::BuildEntry,
    base: *const u8,
    logger: &log::Logger,
    mode: config::AssetServing,
    cap: usize,
) -> TailgateDecision {
    if plan.assets.is_empty() {
        return TailgateDecision {
            declared: BTreeMap::new(),
            tailgated: BTreeSet::new(),
            note: String::new(),
        };
    }
    // hosts in the resolved op order (first appearance)
    let hosts = tailgate_hosts(plan);
    if mode == config::AssetServing::Merge {
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

    match plan_tailgate(plan, exe_dir, entry, base, logger, &hosts, cap) {
        Ok(decision) => decision,
        Err(a) => no_serve(a.declared, a.note),
    }
}

/// The body of the decision after the mode/empty checks: gather the declared
/// sizes, the payloads, the shipped index and the funnel/cave anchors, then
/// plan the per-host sites and the i.dat site and install them. Each `?`-style
/// abandonment carries the declared sizes read so far plus its loud reason.
fn plan_tailgate(
    plan: &Plan,
    exe_dir: &Path,
    entry: &builds::BuildEntry,
    base: *const u8,
    logger: &log::Logger,
    hosts: &[String],
    cap: usize,
) -> Result<TailgateDecision, Abandon> {
    // 1-4. the inputs: declared sizes, payloads, the shipped index + its
    //      collision check, and the two shared anchors
    let TailgateInputs {
        declared,
        blobs,
        decode_rva,
        cave_rva,
        cave_expect,
    } = gather_tailgate_inputs(plan, exe_dir, entry, hosts)?;

    // 5. per host, the ordered payload list = the host's [[asset]] payloads
    //    (resolved order) then its auto-merged .dir listings — EXACTLY the
    //    order `overlay::host_ops` uses, so the rows and the appends cannot
    //    disagree. The listing payloads were already rebuilt at boot
    //    (plan_listings); the asset payloads are the blobs read in step 2.
    let (mut per_host, rows) = build_host_items(plan, hosts, blobs, &declared);

    // 6. per host: budget-check and build the site plan
    let (mut sites, mut merged) =
        plan_host_sites(hosts, &mut per_host, &declared, cap, entry, logger);

    // 7. the i.dat site: records appended at |V_idat| + the count bump.
    //    The rows are appended at STAGE 1, before the stage-2 host decodes, so
    //    i.dat may only tailgate when EVERY host does: if any host is served
    //    by the merge, the rows must come from the merged index too (its
    //    atomicity refuses the index outright when a host cache fails — a
    //    runtime-appended row whose host then serves vanilla would dangle past
    //    the vanilla image).
    plan_idat_site(&mut sites, &mut merged, &declared, &rows, entry, logger);

    // 8. install (one cave, every site) — the cave failure is the only fatal one
    let tailgated = install_sites(decode_rva, cave_rva, cave_expect, sites, base, logger);

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
    Ok(TailgateDecision {
        declared,
        tailgated,
        note,
    })
}

/// The tailgate plan's gather phase (steps 1-4): the per-host vanilla declared
/// sizes, every `[[asset]]` payload read once, and the two shared anchors.
struct TailgateInputs {
    declared: BTreeMap<String, usize>,
    blobs: Vec<Vec<u8>>,
    decode_rva: usize,
    cave_rva: usize,
    cave_expect: Vec<u8>,
}

/// Gather steps 1-4. Any failure carries the declared sizes read so far plus
/// its loud reason (the caller turns it into a merge decision).
fn gather_tailgate_inputs(
    plan: &Plan,
    exe_dir: &Path,
    entry: &builds::BuildEntry,
    hosts: &[String],
) -> Result<TailgateInputs, Abandon> {
    // 1. vanilla declared sizes — the append base for every host and i.dat
    let mut declared: BTreeMap<String, usize> = BTreeMap::new();
    if let Err(note) = read_declared_sizes(exe_dir, hosts, &mut declared) {
        return Err(Abandon { declared, note });
    }

    // 2. every payload, read once (the tailgate appends from memory; the rows
    //    need every payload's length, even for hosts that end up merged)
    let mut blobs: Vec<Vec<u8>> = Vec::with_capacity(plan.assets.len());
    if let Err(note) = read_asset_blobs(plan, &mut blobs) {
        return Err(Abandon { declared, note });
    }

    // 3. the shipped i.dat: the tailgate bypasses merge_idat, so the
    //    authoritative collision check runs here (a collision without
    //    override_vanilla hands the whole thing to merge, which refuses it)
    let shipped = match read_shipped_idat(exe_dir) {
        Ok(i) => i,
        Err(e) => {
            return Err(Abandon {
                declared,
                note: format!("asset serving: merge — tailgate unavailable ({e})"),
            })
        }
    };
    if let Err(note) = check_shipped_collisions(plan, &shipped) {
        return Err(Abandon { declared, note });
    }

    // 4. the funnel + the shared relay cave must be anchored
    let (decode_rva, cave_rva, cave_expect) = match resolve_funnel_cave(entry) {
        Ok(v) => v,
        Err(note) => return Err(Abandon { declared, note }),
    };
    Ok(TailgateInputs {
        declared,
        blobs,
        decode_rva,
        cave_rva,
        cave_expect,
    })
}

/// Step 1: read every host's (and `i.dat`'s) vanilla family-A declared size.
/// On failure `declared` keeps whatever was read so far and the note names the
/// unreadable file.
fn read_declared_sizes(
    exe_dir: &Path,
    hosts: &[String],
    declared: &mut BTreeMap<String, usize>,
) -> Result<(), String> {
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
    let mut keys = hosts.to_vec();
    keys.push("i.dat".to_string());
    for key in &keys {
        match read_declared(key) {
            Ok(d) => {
                declared.insert(key.clone(), d);
            }
            Err(e) => return Err(format!("asset serving: merge — tailgate unavailable ({e})")),
        }
    }
    Ok(())
}

/// Step 2: read every `[[asset]]` payload once, in resolved order.
fn read_asset_blobs(plan: &Plan, blobs: &mut Vec<Vec<u8>>) -> Result<(), String> {
    for a in &plan.assets {
        match std::fs::read(a.mod_dir.join(&a.file)) {
            Ok(b) => blobs.push(b),
            Err(e) => {
                return Err(format!(
                    "asset serving: merge — mod {} asset {} payload unreadable ({e})",
                    a.mod_id, a.name
                ))
            }
        }
    }
    Ok(())
}

/// Step 3b: refuse a name that exists in the shipped index without
/// `override_vanilla` — the merge refuses that loudly, so the whole tailgate
/// hands over to it.
fn check_shipped_collisions(plan: &Plan, shipped: &a9_formats::idat::Idat) -> Result<(), String> {
    for a in &plan.assets {
        if shipped.row(&a.name).is_some() && !a.override_vanilla {
            return Err(format!(
                "asset serving: merge — [[asset]] {:?} collides with a shipped i.dat \
                 row without override_vanilla (merge refuses it loudly)",
                a.name
            ));
        }
    }
    Ok(())
}

/// Step 4: resolve the two shared anchors — the family-A decode funnel and the
/// shared relay cave — requiring non-empty expect bytes on each.
fn resolve_funnel_cave(entry: &builds::BuildEntry) -> Result<(usize, usize, Vec<u8>), String> {
    let resolve = |name: &str| -> Option<(usize, Vec<u8>)> {
        let a = entry.anchor(name)?;
        let expect = a.expect_bytes().filter(|e| !e.is_empty())?;
        Some((a.rva as usize, expect))
    };
    let (Some((decode_rva, _)), Some((cave_rva, cave_expect))) =
        (resolve("pack_lzss_decode"), resolve("pack_lzss_cave"))
    else {
        return Err(
            "asset serving: merge — this build lacks the pack_lzss_decode or pack_lzss_cave \
             anchor (or its expect bytes)"
                .into(),
        );
    };
    Ok((decode_rva, cave_rva, cave_expect))
}

/// Step 5: assemble the per-host ordered item lists (assets then listings) and
/// the i.dat rows from the shared planner (offsets == the host appends).
fn build_host_items(
    plan: &Plan,
    hosts: &[String],
    mut blobs: Vec<Vec<u8>>,
    declared: &BTreeMap<String, usize>,
) -> (BTreeMap<String, Vec<Item>>, Vec<a9_formats::idat::NewRow>) {
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

    let mut rows: Vec<a9_formats::idat::NewRow> = Vec::new();
    for host in hosts {
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
    (per_host, rows)
}

/// Step 6: per host, budget-check (`cap − declared`) and build the site plan;
/// a host that does not fit or lacks a site anchor joins `merged`.
fn plan_host_sites(
    hosts: &[String],
    per_host: &mut BTreeMap<String, Vec<Item>>,
    declared: &BTreeMap<String, usize>,
    cap: usize,
    entry: &builds::BuildEntry,
    logger: &log::Logger,
) -> (Vec<crate::tailgate::SitePlan>, Vec<String>) {
    let mut sites: Vec<crate::tailgate::SitePlan> = Vec::new();
    let mut merged: Vec<String> = Vec::new();
    for host in hosts {
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
    (sites, merged)
}

/// Step 7: the i.dat site. `i.dat` joins the merge path whenever any host did
/// (atomicity), or when its rows do not fit the workspace, or when the build
/// has no i.dat site anchor; otherwise it tailgates with the count bump.
fn plan_idat_site(
    sites: &mut Vec<crate::tailgate::SitePlan>,
    merged: &mut Vec<String>,
    declared: &BTreeMap<String, usize>,
    rows: &[a9_formats::idat::NewRow],
    entry: &builds::BuildEntry,
    logger: &log::Logger,
) {
    let idat_blob = a9_formats::idat::render_rows(rows);
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
    } else if let Some((site_rva, site_expect)) = resolve_site(entry, "pack_lzss_idat_call") {
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
}

/// Resolve one site anchor to `(rva, expect)`, requiring non-empty expect bytes.
fn resolve_site(entry: &builds::BuildEntry, name: &str) -> Option<(usize, Vec<u8>)> {
    let a = entry.anchor(name)?;
    let expect = a.expect_bytes().filter(|e| !e.is_empty())?;
    Some((a.rva as usize, expect))
}

/// Step 8: install the shared cave + every site; returns the pack keys actually
/// spliced (empty when the cave install fails — the only fatal outcome).
fn install_sites(
    decode_rva: usize,
    cave_rva: usize,
    cave_expect: Vec<u8>,
    sites: Vec<crate::tailgate::SitePlan>,
    base: *const u8,
    logger: &log::Logger,
) -> BTreeSet<String> {
    if sites.is_empty() {
        return BTreeSet::new();
    }
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
}
