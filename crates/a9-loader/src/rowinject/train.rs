use super::car::{apply_form_row, build_cars};
use super::diag::{
    CONSIST_TRAMP, CREATE_TRAMP, install_consist_create, install_consist_probe,
    install_panic_capture, probe_row_map,
};
use super::spec::{
    ANCHOR_ACCESSOR, ANCHOR_CONSIST_COPY, ANCHOR_CONSIST_CREATE, ANCHOR_NAME_CTOR, ANCHOR_REALLOC,
    AccessorFn, MANAGER_RVA, MGR_ROWS, NAME_LANE_STRIDE, NAME_LANES, NameCtorFn, ROW_BVALID,
    ROW_CARC0UNT, ROW_NAME_BASE, ROW_NAME_EN, ROW_NAME_JP, ROW_NAMEALT_BASE, ROW_PFORMNAME,
    ROW_SPECIAL, ROW_STRIDE, ReallocFn, verify_anchor,
};
use crate::{log, patch, sys};
use a9_mod::builds::BuildEntry;
use core::ffi::c_void;
use std::sync::{Mutex, OnceLock};

// ===========================================================================
// M7-2 — the `[[train]]` spec-row injection.
//
// The train compiler (a9-mod::train) allocates ids + names and derives the CSV
// rows; this is the boot half: it writes the matching spec rows. It differs
// from M7-1 in three ways:
//
//  * the HOOK SITE is `train_list_builder` 0x14019d9c0, not `spec_table_resize`.
//    The pipeline calls the resize at 0x14019c89e and then IMMEDIATELY fills the
//    rows (0x14019c8c0..0x14019d2cf), so at the resize the shipped rows are still
//    uninitialised and cloning one would copy garbage. `train_list_builder` is
//    called from inside the same pipeline at 0x14019d606 — after the fill loop
//    and the `form_id_%d` auto-name tail — and it walks the vector through the
//    loader's composed keep-set, so injecting BEFORE its original runs is the
//    first point where a donor row is fully populated.
//  * MANY rows, each with its own allocated name and donor.
//  * the NAME-POINTER CLONE FIX: the clone is a `memcpy` of the donor row, so
//    every name pointer it shares would be double-freed by the live teardown.
//    Every one of the 18 lanes (`awName[0..8]` @+0xa8, `awNameAlt[0..8]` @+0xf0)
//    is cloned through `wstr_clone_ref` 0x14001d7d0 (a refcount bump), and the
//    three lanes we set are then re-formatted — the format sees refcount >= 2,
//    so it copy-on-writes and the donor's strings are never mutated.
// ===========================================================================

/// The purchase-list builder — the M7-2 injection site (see above).
pub const ANCHOR_TLB: &str = "train_list_builder";
/// The preview annotator's loop bound (0x1403B0A7F, `CMP EBP,0x1a1`). Widening
/// its immediate to the live row count is what lets an injected row get a
/// rolling-stock list icon at all: the annotator composes the preview names for
/// every valid row it walks, and the shipped literal stops at 417.
pub const ANCHOR_ANNOT_BOUND: &str = "preview_annotator_bound";

/// The consist builder's form-id clamp (`MOV R12D, 0x1a1` in `roster_row_builder`
/// 0x140060f50, imm32 at 0x140060fcc). Every car entry the game writes is
/// `min(candidate, 0x1a1)`, so an injected train's cars resolve through
/// `spec[417]` instead of its own row — that is why a mod train rendered
/// FORM_205's mesh (spec row 3 is not involved; spec[417] holds the 205 models
/// for the aux values the donor clone carries). One site serves purchase,
/// save-load, preview and the readout cluster.
pub const ANCHOR_ROSTER_CLAMP: &str = "roster_form_clamp";
/// The car-model loader the build calls — the GENERIC branch of the pattern
/// loop's ladder, i.e. what the game itself uses for an ordinary car:
/// `train_model_fetch` 0x19B9F0(wrapper, &mesh_path_cell, pattern_row, flag).
/// CALLED, never hooked: it is the same call the game makes, so the object it
/// creates is a first-class one. Returns 1/0 in AL and stores the object in
/// `wrapper[0]`.
pub const ANCHOR_MODEL_FETCH: &str = "train_model_fetch";
/// The car build's helpers (called, never hooked).
pub const ANCHOR_MODEL_REGISTER: &str = "model_register";
pub const ANCHOR_MODEL_INDEX: &str = "model_index_alloc";
/// The empty-string constructor the pipeline calls on a stack local before the
/// formatting constructor assigns into it (0x14019CAE5 -> 0x14001D940). Both
/// constructors ASSIGN, so the destination must exist first.
pub const ANCHOR_RCSTR_CTOR: &str = "rcstr_ctor";
/// `CString` field clone (refcount bump) — the name-pointer clone fix.
pub const ANCHOR_CLONE_REF: &str = "wstr_clone_ref";
/// The shipped FORM-name literal (donor name -> id resolution).
pub const ANCHOR_FORM_LIST: &str = "form_list";

type CloneRefFn = unsafe extern "system" fn(*mut *const u16, *const *const u16);
type TlbFn = unsafe extern "system" fn(*mut u8);

/// One train to inject (built by the engine from `a9_mod::train::CompiledTrain`).
pub struct TrainRow {
    pub id: usize,
    pub form_name: String,
    pub name_jp: String,
    pub name_en: String,
    /// the donor shipped FORM name (resolved to an id at install time)
    pub donor: String,
    pub cruise_speed: u16,
    pub reverse_speed: Option<u16>,
    pub fare: u16,
    pub cars: u8,
    /// the car model + the pattern-row profile the car build passes to the game's
    /// own model loader (see `carbuild`)
    pub model_dir: String,
    pub model_mesh: String,
    pub capacity: u32,
    pub running_cost: u64,
    pub price: u64,
    /// the train's `train_form.csv` row (the vanilla columns) — what the loader
    /// writes into the spec row, so a mod that ships its own CSV rows controls
    /// every field the game parses (see `apply_form_row`)
    pub form_cols: Vec<String>,
    /// the train's `train_pattern.csv` group — the car types and their coupling
    /// columns, which drive the car build (one model per modelled row, the
    /// per-car block, the consist length)
    pub pattern_rows: Vec<Vec<String>>,
}

/// Immutable install-time state for the train injection.
pub(super) struct TrainState {
    pub(super) logger: log::Logger,
    pub(super) base: usize,
    pub(super) realloc: usize,
    pub(super) accessor: usize,
    pub(super) name_ctor: usize,
    pub(super) clone_ref: usize,
    /// the preview annotator's bound site (optional: a missing anchor or a byte
    /// mismatch only costs the injected rows their list icon)
    pub(super) annot_bound: Option<u32>,
    pub(super) roster_clamp: Option<u32>,
    /// the car build's resolved addresses (all optional: without them the
    /// injected rows keep the donor's car data, as before). All four are CALLED,
    /// never hooked.
    pub(super) model_fetch: Option<usize>,
    pub(super) model_register: Option<usize>,
    pub(super) model_index_alloc: Option<usize>,
    pub(super) rcstr_ctor: Option<usize>,
    /// opt-in: drive the game's car-model loader (a9loader.toml `car_build`)
    pub(super) car_build: bool,
    /// the car build waits for a LATER builder call: later calls come after the
    /// boot (e.g. when a scenario is loaded), when no pack load is in flight.
    pub(super) car_built: bool,
    /// DIAGNOSTIC (opt-in): the per-`train_list_builder` spec-row dump (`probe_rows`).
    pub(super) probe_rows: bool,

    /// one resolved (donor_id, TrainRow) per train, in allocation order
    pub(super) rows: Vec<(usize, TrainRow)>,
    pub(super) tramp: Option<patch::Trampoline>,
    pub(super) done: bool,
    /// how many `train_list_builder` calls have been seen, and whether the
    /// "not ready yet" warning has been emitted (once per boot)
    pub(super) attempts: u32,
    pub(super) warned: bool,
}

static TG: OnceLock<Mutex<TrainState>> = OnceLock::new();

/// `L"%s"` — the format every string assignment through `row_name_ctor` uses, with
/// the text itself passed as the vararg. The constructor is printf-style: passing
/// the *text* as the format (what the first version did) works for a name without
/// a `%`, but a name like `100% Express` would then be read as a conversion
/// specification and dereference a vararg we never passed. A static format plus a
/// pointer argument makes author text inert, exactly like the car build's cells.
static PCT_S: [u16; 3] = [0x25, 0x73, 0];

/// Read the live FORM list and resolve `name` to its position (id).
unsafe fn form_id(entry: &BuildEntry, base: usize, name: &str) -> Option<usize> {
    unsafe {
        let a = entry.anchor(ANCHOR_FORM_LIST)?;
        let units =
            crate::handlers::read_u16s_bounded(base as *const u8, a.rva as usize, 16384).ok()?;
        crate::forms::parse_form_list(&units)
            .iter()
            .position(|f| f == name)
    }
}

/// Install the train injection: expect-verify every anchor, resolve each
/// train's donor to a live id, and hook `train_list_builder` (trampoline-only —
/// the original still runs, after the injection).
pub unsafe fn install_trains(
    entry: &BuildEntry,
    base: *const u8,
    logger: log::Logger,
    trains: &[TrainRow],
    cfg_car_build: bool,
    cfg_probe_consist: bool,
    cfg_probe_rows: bool,
) -> Result<String, String> {
    unsafe {
        if trains.is_empty() {
            return Err("no trains to inject".into());
        }
        // DIAGNOSTIC: make a loader panic readable (see install_panic_capture).
        install_panic_capture(&logger);
        for name in [
            ANCHOR_REALLOC,
            ANCHOR_ACCESSOR,
            ANCHOR_NAME_CTOR,
            ANCHOR_CLONE_REF,
            ANCHOR_TLB,
        ] {
            verify_anchor(entry, base as usize, name)?;
        }
        let realloc = entry.anchor(ANCHOR_REALLOC).unwrap();
        let accessor = entry.anchor(ANCHOR_ACCESSOR).unwrap();
        let ctor = entry.anchor(ANCHOR_NAME_CTOR).unwrap();
        let clone_ref = entry.anchor(ANCHOR_CLONE_REF).unwrap();
        let tlb = entry.anchor(ANCHOR_TLB).unwrap();

        // resolve donors BEFORE any hook: an unresolvable donor refuses the
        // whole install (fail-open — the game boots with no train rows).
        let mut rows = Vec::with_capacity(trains.len());
        for t in trains {
            let donor_id = form_id(entry, base as usize, &t.donor).ok_or_else(|| {
                format!(
                    "train '{}': donor {:?} is not a shipped FORM name in this build's \
                     catalog — refusing (fail-open)",
                    t.form_name, t.donor
                )
            })?;
            rows.push((
                donor_id,
                TrainRow {
                    id: t.id,
                    form_name: t.form_name.clone(),
                    name_jp: t.name_jp.clone(),
                    name_en: t.name_en.clone(),
                    donor: t.donor.clone(),
                    cruise_speed: t.cruise_speed,
                    reverse_speed: t.reverse_speed,
                    fare: t.fare,
                    cars: t.cars,
                    model_dir: t.model_dir.clone(),
                    model_mesh: t.model_mesh.clone(),
                    capacity: t.capacity,
                    running_cost: t.running_cost,
                    price: t.price,
                    form_cols: t.form_cols.clone(),
                    pattern_rows: t.pattern_rows.clone(),
                },
            ));
        }

        let tlb_expect = tlb.expect_bytes().unwrap_or_default();
        if tlb_expect.len() < 12 {
            return Err(format!(
                "{ANCHOR_TLB} records {} expect bytes, the hook needs >= 12",
                tlb_expect.len()
            ));
        }
        // Optional: the preview annotator's 417-row loop bound. A missing anchor
        // (an older address book) or a byte mismatch only costs the icons, so it
        // never refuses the injection.
        let roster_clamp = match entry.anchor(ANCHOR_ROSTER_CLAMP) {
            Some(a) => match verify_anchor(entry, base as usize, ANCHOR_ROSTER_CLAMP) {
                Ok(()) => Some(a.rva),
                Err(e) => {
                    logger.warn(&format!(
                        "train injection: {ANCHOR_ROSTER_CLAMP} failed its byte check ({e}) — \
                         injected trains keep the 417 clamp and render the overflow row's mesh"
                    ));
                    None
                }
            },
            None => {
                logger.warn(&format!(
                    "train injection: the address book carries no {ANCHOR_ROSTER_CLAMP} anchor — \
                     injected trains keep the 417 clamp and render the overflow row's mesh"
                ));
                None
            }
        };
        let annot_bound = match entry.anchor(ANCHOR_ANNOT_BOUND) {
            Some(a) => match verify_anchor(entry, base as usize, ANCHOR_ANNOT_BOUND) {
                Ok(()) => Some(a.rva),
                Err(e) => {
                    logger.warn(&format!(
                        "train injection: {ANCHOR_ANNOT_BOUND} failed its byte check ({e}) — \
                         injected rows will show a blank list icon"
                    ));
                    None
                }
            },
            None => {
                logger.warn(&format!(
                    "train injection: the address book carries no {ANCHOR_ANNOT_BOUND} anchor — \
                     injected rows will show a blank list icon"
                ));
                None
            }
        };
        // The car build's four helpers are CALLED, never hooked — so verify their
        // bytes now and only keep the ones that match, like every hooked site: a
        // shifted build costs the car data (and one warning), never a call into an
        // unknown instruction.
        let helper = |name: &str| -> Option<usize> {
            match entry.anchor(name) {
                Some(a) => match verify_anchor(entry, base as usize, name) {
                    Ok(()) => Some(a.rva as usize),
                    Err(e) => {
                        logger.warn(&format!(
                            "car build: {name} failed its byte check ({e}) — a [[train]] keeps \
                             the donor's car data (capacity / running cost / price)"
                        ));
                        None
                    }
                },
                None => {
                    logger.warn(&format!(
                        "car build: the address book carries no {name} anchor — a [[train]] keeps \
                         the donor's car data (capacity / running cost / price)"
                    ));
                    None
                }
            }
        };
        let model_fetch = helper(ANCHOR_MODEL_FETCH).map(|r| base as usize + r);
        let model_register = helper(ANCHOR_MODEL_REGISTER).map(|r| base as usize + r);
        let model_index_alloc = helper(ANCHOR_MODEL_INDEX).map(|r| base as usize + r);
        let rcstr_ctor = helper(ANCHOR_RCSTR_CTOR).map(|r| base as usize + r);
        let _ = TG.set(Mutex::new(TrainState {
            logger: logger.clone(),
            base: base as usize,
            realloc: base as usize + realloc.rva as usize,
            accessor: base as usize + accessor.rva as usize,
            name_ctor: base as usize + ctor.rva as usize,
            clone_ref: base as usize + clone_ref.rva as usize,
            annot_bound,
            roster_clamp,
            model_fetch,
            model_register,
            model_index_alloc,
            rcstr_ctor,
            car_build: cfg_car_build,
            car_built: false,
            probe_rows: cfg_probe_rows,
            rows,
            tramp: None,
            done: false,
            attempts: 0,
            warned: false,
        }));
        let m = TG.get().unwrap();
        let mut g = m
            .lock()
            .map_err(|_| "train state lock poisoned".to_string())?;
        let entry_ptr = (base as usize + tlb.rva as usize) as *mut u8;
        let t = patch::install_trampoline(
            entry_ptr,
            my_train_list_builder as *const c_void,
            &tlb_expect,
            sys::PAGE_EXECUTE_READWRITE,
        )?;
        let n = g.rows.len();
        let rva = tlb.rva;
        g.tramp = Some(t);
        // DIAGNOSTIC: the consist copier (see `my_consist_copy`). Installed here
        // because this is the path the live game takes — `ensure(true, ..)` only runs
        // from the harness export, which is why the first attempt never fired.
        if cfg_probe_consist && CONSIST_TRAMP.load(core::sync::atomic::Ordering::Relaxed) == 0 {
            if let Some(a) = entry.anchor(ANCHOR_CONSIST_COPY) {
                let expect = a.expect_bytes().unwrap_or_default();
                drop(g);
                install_consist_probe(&logger, base as *const u8, a.rva as usize, &expect);
            }
        }
        if cfg_probe_consist && CREATE_TRAMP.load(core::sync::atomic::Ordering::Relaxed) == 0 {
            if let Some(a) = entry.anchor(ANCHOR_CONSIST_CREATE) {
                let expect = a.expect_bytes().unwrap_or_default();
                install_consist_create(&logger, base as *const u8, a.rva as usize, &expect);
            }
        }
        let live = [
            ("train_model_fetch", model_fetch.is_some()),
            ("model_register", model_register.is_some()),
            ("model_index_alloc", model_index_alloc.is_some()),
            ("rcstr_ctor", rcstr_ctor.is_some()),
        ]
        .iter()
        .filter(|(_, ok)| *ok)
        .count();
        logger.debug(&format!("car build: {live}/4 helper anchors verified"));
        Ok(format!(
            "train injection: train_list_builder rva={rva:#x} expect=verified \
             ({n} reserved row(s) queued; inject before the builder walks the vector)"
        ))
    }
}
/// The `train_list_builder` hook: inject once (fail-open), then run the
/// original so it walks the grown vector through the composed keep-set.
///
/// The builder is called more than once per boot, and the FIRST call happens
/// before the CSV pipeline has populated the spec rows — a live boot proved it
/// (`donor row 195 (FORM_KH32) is not populated yet (bValid == 0)`), which the
/// harness could not see because its fake host only ever calls the site once
/// with the rows already populated. So a refusal is treated as "not ready yet":
/// it is logged once at warn level and RETRIED at the next call, and the
/// injection lands on the first call whose list build can actually include the
/// row. Structural failures therefore cost a warning per boot rather than a
/// silent nothing.
unsafe extern "system" fn my_train_list_builder(manager: *mut u8) {
    unsafe {
        // inject once, WITHOUT the lock held across the original
        {
            let Some(m) = TG.get() else { return };
            let Ok(mut g) = m.lock() else { return };
            g.attempts += 1;
            // DIAGNOSTIC: dump the spec-table state at EVERY call, including the
            // ones after a successful injection, so the log shows how readiness
            // evolves across the boot.
            if g.probe_rows {
                probe_row_map(&g, g.attempts, manager);
            }
            if !g.done {
                match inject_trains_locked(&g) {
                    Ok(n) => {
                        g.done = true;
                        let logger = g.logger.clone();
                        let attempts = g.attempts;
                        logger.line(&format!(
                            "train injection: grew the spec vector by {n} row(s) and \
                             wrote the reserved rows through the game's own realloc \
                             (attempt {attempts} of the builder hook)"
                        ));
                        // The car build runs in THIS call, i.e. in the same pack-load
                        // window as the injection. The game's model loader resolves the
                        // mesh out of the pack that is resident right now (the vp
                        // pipeline); a later builder call — the one that arrives on a
                        // scenario load — has no pack window left, the loader cannot
                        // find the mesh and refuses the car (live log 2026-10-15: the
                        // deferred build returned 0 and the shop kept the donor's
                        // numbers). It must also run before the original builder below,
                        // which reads the row's car data for the purchase list.
                        if !g.car_built {
                            g.car_built = true;
                            let outcome = if g.car_build {
                                build_cars(&g)
                            } else {
                                Err("disabled (set car_build = true in a9loader.toml \
                                     to test)"
                                    .into())
                            };
                            match outcome {
                                Ok(n) if n > 0 => {
                                    g.logger.line(&format!(
                                        "car build: {n} car(s) loaded through the game's \
                                         own model loader and written into the reserved \
                                         row(s) (builder call {attempts})"
                                    ));
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    g.logger.line(&format!("car build skipped: {e}"));
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let logger = g.logger.clone();
                        logger.debug(&format!(
                            "train injection: not ready at builder call {}: {e} — will \
                             retry at the next call",
                            g.attempts
                        ));
                        if !g.warned {
                            g.warned = true;
                            logger.warn(&format!(
                                "train injection deferred: {e} — retrying at the next \
                                 train_list_builder call; if the reserved rows never \
                                 appear, no call found the spec rows populated"
                            ));
                        }
                    }
                }
            }
        }
        let orig = TG.get().and_then(|m| {
            m.lock()
                .ok()
                .and_then(|g| g.tramp.as_ref().map(|t| t.as_ptr() as usize))
        });
        if let Some(p) = orig {
            let f: TlbFn = core::mem::transmute(p);
            f(manager);
        }
    }
}

/// The M7-2 harness seam: dump the four train tables out of the MERGED csvp
/// cache (`<mods_dir>/cache/csvp.bin`), one line per row. The harness greps the
/// derived rows to prove they reached the table through the real overlay merge
/// (the positional `train_pattern` group append included).
///
/// Line shape: `<table> <row#> key=<key> fields=<f0|f1|...>` (fields joined by
/// `|`, decoded lossily to UTF-8). Returns the total row count, or 0 on any
/// refusal.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn a9loader_m7_dump() -> u64 {
    let Ok(exe) = std::env::current_exe() else {
        return 0;
    };
    let Some(exe_dir) = exe.parent() else {
        return 0;
    };
    let cfg = a9_mod::config::read(&exe_dir.join("a9loader.toml"));
    let cache = exe_dir.join(&cfg.mods_dir).join("cache").join("csvp.bin");
    let Ok(file) = std::fs::read(&cache) else {
        return 0;
    };
    let Ok(decoded) = a9_formats::pack::decode_pack(&file, a9_formats::pack::Family::B) else {
        return 0;
    };
    let Ok(entries) = a9_formats::container::parse_csvs(&decoded) else {
        return 0;
    };
    let logger = log::Logger::new(&exe_dir.join(&cfg.mods_dir).join("a9loader.log"));
    let mut total = 0u64;
    for want in [
        "train_form.csv",
        "train_pattern.csv",
        "train_comment.csv",
        "train_custom.csv",
    ] {
        let Some(e) = entries.iter().find(|e| e.name.as_str() == want) else {
            continue;
        };
        let Ok(t) = a9_formats::csv::Table::parse(&e.payload) else {
            continue;
        };
        for (n, row) in t.rows.iter().enumerate() {
            logger.line(&format!(
                "m7dump {} #{} key={} fields={}",
                want,
                n,
                row.key(),
                row.fields.join("|")
            ));
            total += 1;
        }
    }
    total
}
/// The core: grow the vector by `rows.len()` (through the engine's realloc) and
/// write each reserved row. Refuses (no mutation) unless the vector is at the
/// expected base and every donor row is populated.
unsafe fn inject_trains_locked(g: &TrainState) -> Result<usize, String> {
    unsafe {
        let n = g.rows.len();
        let vec = (g.base + MANAGER_RVA + MGR_ROWS) as *mut u8;
        if !sys::is_readable(vec as *const c_void, 0x18) {
            return Err("the spec-vector object is unreadable".into());
        }
        let begin = *(vec as *const usize);
        let end = *((vec as *const usize).add(1));
        let cap = *((vec as *const usize).add(2));
        if begin == 0 || end < begin || cap < end {
            return Err("spec-vector fields nonsensical".into());
        }
        let count = (end - begin) / ROW_STRIDE;
        // the expected base: the first reserved id must be the live count
        let first = g.rows.iter().map(|(_, r)| r.id).min().unwrap_or(0);
        if count != first {
            return Err(format!(
                "the spec vector has {count} rows, expected {first} — the train \
                 rows were already injected or the table is not what we think it is"
            ));
        }
        let accessor: AccessorFn = core::mem::transmute(g.accessor);
        let clone_ref: CloneRefFn = core::mem::transmute(g.clone_ref);
        let ctor: NameCtorFn = core::mem::transmute(g.name_ctor);

        // build every row in scratch FIRST (a failure refuses before any growth)
        let mut scratches: Vec<[u8; ROW_STRIDE]> = Vec::with_capacity(n);
        for (donor_id, t) in &g.rows {
            let donor = accessor(vec, *donor_id);
            if donor.is_null() || !sys::is_readable(donor as *const c_void, ROW_STRIDE) {
                return Err(format!(
                    "train '{}': donor row {donor_id} is unreadable",
                    t.form_name
                ));
            }
            if *donor.add(ROW_BVALID) == 0 {
                return Err(format!(
                    "train '{}': donor row {donor_id} ({}) is not populated (bValid == 0) — \
                     the game never enabled that train in this build, so there is no row to \
                     clone. Pick a donor that appears in the purchase list (FORM_AR4 is in \
                     the shipped whitelist); a shipped train can still be disabled here, \
                     FORM_KH32 among them",
                    t.form_name, t.donor
                ));
            }
            let mut s = [0u8; ROW_STRIDE];
            core::ptr::copy_nonoverlapping(donor, s.as_mut_ptr(), ROW_STRIDE);
            // The donor row is cloned mid-pipeline, before the game has finished
            // initialising it: the padding above the validity byte still held recycled
            // heap bytes there (live 2026-10-15: `0d 0a 3a` — text from a freed
            // buffer, and the game crashed when the train was selected). Every shipped
            // row carries the whole u32 == 1, and a row outside the pipeline's 417-row
            // fill loop never has it rewritten, so set it explicitly.
            core::ptr::write_unaligned(s.as_mut_ptr() as *mut u32, 1);
            // 1) clone every shared name lane (refcount bump) so the row owns them.
            //
            // `pFormName` (+0x138) is one of these shared strings, not a separate
            // buffer: it is a single pointer, but the game's assign-style name
            // constructor COWs it exactly like a lane. Leaving it out of the bump
            // made the FIRST clone write into the donor's own object (the refcount
            // was 1, so no copy happened), which then made the SECOND clone's write
            // on that same object throw an unhandled C++ exception inside the game's
            // `cstring_format_2args` — live 2026-10-15, the two-train mod died on
            // `row 422 lane +0x138 text "form_train_acceptance_csvtrain"`. With the
            // bump every clone COWs and the donor stays intact.
            for (base_off, lanes) in [
                (ROW_NAME_BASE, NAME_LANES),
                (ROW_NAMEALT_BASE, NAME_LANES),
                (ROW_PFORMNAME, 1),
            ] {
                for k in 0..lanes {
                    let off = base_off + k * NAME_LANE_STRIDE;
                    let field = s.as_mut_ptr().add(off) as *mut *const u16;
                    if (*field).is_null() {
                        continue;
                    }
                    clone_ref(field, field as *const *const u16);
                }
            }
            // 2) re-format the three lanes we set (COW: refcount >= 2)
            //
            // pFormName is LOWERCASE on purpose. Every shipped row carries it that
            // way ("form_kh32", "form_285" — read live with the train probe), and
            // the game derives asset lookups from it: the preview trio is
            // `<pFormName>_f.dds` / `_s.dds` / `_s_1.dds`, which is exactly how the
            // compiler names the icon payloads. An uppercase lane made the game ask
            // for "FORM_…_f.dds", which no listing and no payload provides — hence
            // the blank preview icons on the first live acceptance boot.
            let jp: Vec<u16> = t.name_jp.encode_utf16().chain([0]).collect();
            let en: Vec<u16> = t.name_en.encode_utf16().chain([0]).collect();
            let form: Vec<u16> = t
                .form_name
                .to_lowercase()
                .encode_utf16()
                .chain([0])
                .collect();
            // The Alt lanes 0 and 1 are what the Rolling-Stock panel prints as its
            // title: the live probe read the donor's as "ＡＲ４－アートディンク" (JP)
            // and "AR4 - ARTDINK" (EN) — a language pair — and the panel shows the
            // EN one in English mode. Clearing only lane 0 left the EN lane intact,
            // which is why the title did not change. Clear both so the panel falls
            // back to the row's own display name instead of crediting the donor's
            // manufacturer. (A `manufacturer` field belongs with the author-declared
            // CSV work.)
            let title: Vec<u16> = [0u16].to_vec();
            for (off, text) in [
                (ROW_NAME_JP, &jp),
                (ROW_NAME_EN, &en),
                (ROW_PFORMNAME, &form),
                (ROW_NAMEALT_BASE, &title),
                (ROW_NAMEALT_BASE + 0x8, &title),
            ] {
                ctor(
                    s.as_mut_ptr().add(off) as *mut *const u16,
                    PCT_S.as_ptr(),
                    text.as_ptr() as usize,
                );
            }
            for (off, what) in [
                (ROW_NAME_JP, "JP display lane"),
                (ROW_NAME_EN, "EN display lane"),
                (ROW_PFORMNAME, "pFormName"),
                (ROW_NAMEALT_BASE, "panel title lane (JP)"),
                (ROW_NAMEALT_BASE + 0x8, "panel title lane (EN)"),
            ] {
                if (*(s.as_ptr().add(off) as *const *const u16)).is_null() {
                    return Err(format!(
                        "train '{}': the name constructor produced a NULL {what}",
                        t.form_name
                    ));
                }
            }
            // 3) the declared numeric fields (the donor supplies the rest)
            s[ROW_BVALID] = 1;
            s[ROW_SPECIAL] = 0;
            // the train's own train_form row (vanilla columns) drives every field
            apply_form_row(s.as_mut_ptr(), &t.form_cols);
            s[ROW_CARC0UNT] = t.cars;
            scratches.push(s);
        }

        // grow through the engine's own reallocate, then append the rows
        let target = count + n;
        let realloc: ReallocFn = core::mem::transmute(g.realloc);
        realloc(vec, target);
        let begin2 = *(vec as *const usize);
        if begin2 == 0 {
            return Err("the engine realloc left the vector begin NULL".into());
        }
        *((vec as *const usize).add(1) as *mut usize) = begin2 + target * ROW_STRIDE;
        for (i, s) in scratches.iter().enumerate() {
            let id = count + i;
            let row = accessor(vec, id);
            if row.is_null() || !sys::is_readable(row as *const c_void, ROW_STRIDE) {
                return Err(format!("reserved row {id} is unreadable after the growth"));
            }
            core::ptr::copy_nonoverlapping(s.as_ptr(), row, ROW_STRIDE);
        }

        // Widen the preview annotator's loop bound to the new row count, so the
        // rows we just wrote get their `form_<pFormName>_*.dds` preview names
        // composed. The annotator's sole caller is train_csv_pipeline_builder at
        // 0x14019d630 — 42 bytes after the builder call we hooked — so this lands
        // before it runs. Costs only the icons if it cannot be applied.
        if let Some(rva) = g.annot_bound {
            let site = (g.base + rva as usize) as *mut u8;
            let total = count + n;
            let expect = [0x81u8, 0xfd, 0xa1, 0x01, 0x00, 0x00];
            let mut got = [0u8; 6];
            if sys::is_readable(site as *const c_void, 6) {
                core::ptr::copy_nonoverlapping(site, got.as_mut_ptr(), 6);
            }
            if got != expect {
                g.logger.warn(&format!(
                    "train injection: unexpected bytes {got:02x?} at the preview annotator \
                     bound — the injected rows keep a blank list icon"
                ));
            } else if patch::write_bytes(
                site.add(2),
                &(total as u32).to_le_bytes(),
                sys::PAGE_EXECUTE_READWRITE,
            ) {
                g.logger.line(&format!(
                    "train injection: preview annotator bound widened to {total} rows (was 417) \
                     so the injected rows get their list icons"
                ));
            } else {
                g.logger.warn(
                    "train injection: could not make the preview annotator bound writable — \
                     the injected rows keep a blank list icon",
                );
            }
        }

        // Lift `roster_row_builder`'s form-id clamp so an injected row's id survives
        // into the consist object's car entries. The builder writes every entry as
        // `min(candidate, 0x1a1)`, so without this an injected train's cars all resolve
        // through `spec[417]` — which is exactly the wrong mesh we have been chasing.
        // One in-place 4-byte write (a data immediate, no cave), expect-verified; the
        // bound is our last allocated id, and ids past it still clamp to a valid row.
        if let Some(rva) = g.roster_clamp {
            let site = (g.base + rva as usize) as *mut u8;
            let bound = (count + n).saturating_sub(1) as u32;
            let expect = [0x41u8, 0xbc, 0xa1, 0x01, 0x00, 0x00];
            let mut got = [0u8; 6];
            if sys::is_readable(site as *const c_void, 6) {
                core::ptr::copy_nonoverlapping(site, got.as_mut_ptr(), 6);
            }
            if got != expect {
                g.logger.warn(&format!(
                    "train injection: unexpected bytes {got:02x?} at the roster form clamp — \
                     injected trains keep the 417 clamp and render the overflow row's mesh"
                ));
            } else if patch::write_bytes(
                site.add(2),
                &bound.to_le_bytes(),
                sys::PAGE_EXECUTE_READWRITE,
            ) {
                g.logger.line(&format!(
                    "train injection: roster form clamp lifted to id {bound} (was 417) so an \
                     injected train's car entries name its own row"
                ));
            } else {
                g.logger.warn(
                    "train injection: could not make the roster form clamp writable — injected \
                     trains render the overflow row's mesh",
                );
            }
        }

        Ok(n)
    }
}
