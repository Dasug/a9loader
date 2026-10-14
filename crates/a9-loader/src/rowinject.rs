//! M7-1 — **direct spec-row injection** (the mechanism). Corpus notes/57 §6.2
//! (architecture Q), notes/62 §1/§4, PLAN §M7. M7-2 extends this module with
//! the `[[train]]` path (see the M7-2 block further down): N rows, each with its
//! own allocated name + donor, hooked at `train_list_builder` (after the
//! pipeline's fill loop) instead of `spec_table_resize`, with the name-pointer
//! clone fix. The M7-1 resize-hook path below stays the harness seam
//! (`a9loader_m7_rows`) and is unchanged.
//!
//! The goal (PLAN §M7): add trains BEYOND the 417-name FORM catalog without
//! touching the FORM literal or the boot-time matcher. Post-boot everything
//! addresses spec rows by **index** (`base = [0x140e14528]`, `row = base +
//! id*0x140`), so an injected row's name never has to be in the FORM list. This
//! module is that injection, proven in the wine harness:
//!
//! 1. **grow the vector AFTER `spec_table_resize`** 0x14019f150. That function
//!    re-pins the vector to EXACTLY 421 (`mov ebx,0x1a5`), so a growth done
//!    earlier is silently undone. [`install`] hooks it trampoline-only and
//!    injects after the original returns; [`inject_now`] is the bare primitive
//!    the harness also drives directly (to show the wrong order being undone).
//! 2. the growth itself is [`spec_table_realloc`] 0x14019fc50 `(&vec,
//!    new_total_rows)` — the engine's OWN reallocate, which allocates the new
//!    block with the game's allocator (0x14040a838), copies the live rows,
//!    destroys + frees the old block and sets `{begin,end,cap}`. Using the
//!    engine's realloc is the point: the block is owned by the same allocator
//!    family the vector destructor frees with, so the teardown stays clean (the
//!    M5-T lesson).
//! 3. an injected row is a **CLONE of a shipped template row** (`memcpy` of the
//!    whole 0x140 row) with the fields that matter patched: the JP/EN display
//!    lanes (`row+0xa8`/`row+0xb0` = `awName[0]`/`awName[1]`; the lane is chosen
//!    by `manager+0x90`, 0 = JP / 8 = EN) and `pFormName` (`row+0x138`). The new
//!    strings come from the engine's OWN formatted-name constructor
//!    [`row_name_ctor`] 0x1400372f0 — the same one `train_list_builder`'s tail
//!    uses to auto-name rows 417-420 (`&row->pFormName, L"form_id_%d", id`), so
//!    they are valid A9_RcStr blocks (refcount header at `data-0x18`), not
//!    zero-filled memory.
//! 4. every reserved row is **NAMED with a valid, unique `pFormName`**. Hard
//!    requirement: `save_train_group_restore` 0x1401b6d10's blank path clones
//!    the name via `wstr_clone_data(*(void**)(row+0x138) - 0x18)`, so a
//!    zero-filled reserved row dereferences a wild pointer. The mechanism
//!    refuses (fail-open, loud) when the template's `pFormName` is NULL or when
//!    the name constructor returns NULL for any of the three name fields.
//! 5. row stride is 0x140 and ids are **u16 in the save** (notes/62 §3.1), so
//!    the reserved range must stay ≤ 65535. M7-1 also keeps it under
//!    [`crate::forms::FORMS_MAX`] = 1024 — the composed keep-set bitmap's width —
//!    so the loader can still admit its own trains; a reserved range above that
//!    would need the bitmap widened (M7-2), and the mechanism refuses rather
//!    than inject rows it could never show.
//!
//! Every row address is resolved through the engine's own
//! [`spec_table_row_accessor`] 0x1401aacb0 (`*(vec) + id*0x140`, no bounds
//! check) so the id→row contract is the engine's, not a re-implementation.
//!
//! # Honest limits (see the report)
//!
//! * The M7-1 clone shares the template's *other* name pointers (`awName[2..8]`,
//!   `awNameAlt[0..8]`) without a refcount increment, so the LIVE teardown could
//!   double-free them. The M7-2 `[[train]]` path fixes this with `wstr_clone_ref`
//!   0x14001d7d0 (see below); the M7-1 seam is left as-is (harness-only).
//! * `install` / `inject_now` are driven from the harness through the
//!   [`a9loader_m7_rows`] export; the M7-2 train path IS wired into
//!   `engine::boot` (`install_trains`).

use crate::{log, patch, sys};
use a9_mod::builds::BuildEntry;
use core::ffi::c_void;
use std::sync::{Mutex, OnceLock};

/// `std::vector<A9_SpecRow_140>::resize(421)` — the boot-time re-pin.
pub const ANCHOR_RESIZE: &str = "spec_table_resize";
/// The vector's own reallocate (`(&vec, new_total_rows)`, game allocator).
pub const ANCHOR_REALLOC: &str = "spec_table_realloc";
/// `(rcx = &vec, rdx = id) -> row*` = `*(vec) + id*0x140`.
pub const ANCHOR_ACCESSOR: &str = "spec_table_row_accessor";
/// The builder's formatted-name constructor (`&field, L"form_id_%d", id`).
pub const ANCHOR_NAME_CTOR: &str = "row_name_ctor";

/// The spec-table manager singleton (`DAT_140e14510`).
const MANAGER_RVA: usize = 0xE14510;
/// The manager's rows vector object `{begin,end,cap}` (`manager+0x18`).
const MGR_ROWS: usize = 0x18;
/// The spec-row stride.
const ROW_STRIDE: usize = 0x140;
/// `A9_SpecRow_140` fields (notes/52 §5).
const ROW_BVALID: usize = 0x00;
const ROW_NAME_JP: usize = 0xa8;
const ROW_NAME_EN: usize = 0xb0;
const ROW_PFORMNAME: usize = 0x138;
/// The per-car save id is a u16 (notes/62 §3.1) — the hard upper bound.
const RESERVED_MAX: usize = 65535;

/// The shipped, valid template row this mechanism clones (M7-1 harness: row 0).
const TEMPLATE_ID: usize = 0;
/// How many reserved rows the mechanism injects (M7-1: one).
const RESERVED_ROWS: usize = 1;

/// `L"form_id_%d"` — the engine's own auto-name shape (its literal lives at
/// 0x14070bd88; the text is identical, so the loader keeps its own copy and no
/// game .rdata page is needed).
static FORM_FMT: [u16; 11] = [
    0x66, 0x6f, 0x72, 0x6d, 0x5f, 0x69, 0x64, 0x5f, 0x25, 0x64, 0,
];
/// `L"modrow_%d"` — the injected row's JP/EN display lanes (M7-1 stand-in text).
static JP_FMT: [u16; 10] = [0x6d, 0x6f, 0x64, 0x72, 0x6f, 0x77, 0x5f, 0x25, 0x64, 0];

type ResizeFn = unsafe extern "system" fn(*mut u8);
type ReallocFn = unsafe extern "system" fn(*mut u8, usize);
type AccessorFn = unsafe extern "system" fn(*const u8, usize) -> *mut u8;
type NameCtorFn = unsafe extern "system" fn(*mut *const u16, *const u16, usize);

struct Inner {
    logger: log::Logger,
    base: usize,
    realloc: usize,
    accessor: usize,
    name_ctor: usize,
    resize_rva: usize,
    resize_expect: Vec<u8>,
    resize_tramp: Option<patch::Trampoline>,
    injected: bool,
}

static G: OnceLock<Mutex<Inner>> = OnceLock::new();

/// Resolve the running build's address book entry. In the harness the host exe
/// carries the game's FILEVERSION (harness/host.rc), so this works exactly like
/// the engine's own build gate.
fn resolve_entry() -> Result<BuildEntry, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let bytes = std::fs::read(&exe).map_err(|e| format!("read {}: {e}", exe.display()))?;
    let build = a9_mod::buildid::from_exe_bytes(&bytes)
        .ok_or_else(|| "exe has no VERSIONINFO".to_string())?;
    let book = a9_mod::builds::embedded_book().map_err(|e| format!("address book: {e}"))?;
    book.build
        .get(&build.id)
        .cloned()
        .ok_or_else(|| format!("unknown build {}", build.id))
}

/// Read `len` bytes at `base+rva`, refusing (never crashing) on unreadable
/// memory; `None` when the anchor is absent from the book.
fn anchor_bytes(entry: &BuildEntry, base: usize, name: &str, len: usize) -> Result<Vec<u8>, String> {
    let a = entry
        .anchor(name)
        .ok_or_else(|| format!("this build has no {name} anchor"))?;
    let p = (base + a.rva as usize) as *const c_void;
    if !unsafe { sys::is_readable(p, len) } {
        return Err(format!("{name} rva {:#x} is not readable memory", a.rva));
    }
    Ok(unsafe { std::slice::from_raw_parts(p as *const u8, len) }.to_vec())
}

/// Expect-verify one anchor against the address book (read-only fingerprint).
fn verify_anchor(entry: &BuildEntry, base: usize, name: &str) -> Result<(), String> {
    let a = entry
        .anchor(name)
        .ok_or_else(|| format!("this build has no {name} anchor"))?;
    let expect = a.expect_bytes().unwrap_or_default();
    if expect.is_empty() {
        return Err(format!("{name} has no expect bytes in the address book"));
    }
    let cur = anchor_bytes(entry, base, name, expect.len())?;
    if cur != expect {
        return Err(format!(
            "{name} bytes differ from the address book (game update?) — refusing"
        ));
    }
    Ok(())
}

/// Prepare (once): resolve the build, expect-verify all four anchors, publish
/// the state. `install_hook` additionally installs the `spec_table_resize`
/// trampoline (idempotent).
unsafe fn ensure(install_hook: bool, logger: &log::Logger) -> Result<(), String> {
    unsafe {
        if G.get().is_none() {
            let base = sys::GetModuleHandleW(std::ptr::null()) as usize;
            if base == 0 {
                return Err("GetModuleHandleW(NULL) returned NULL".into());
            }
            let entry = resolve_entry()?;
            for name in [ANCHOR_RESIZE, ANCHOR_REALLOC, ANCHOR_ACCESSOR, ANCHOR_NAME_CTOR] {
                verify_anchor(&entry, base, name)?;
            }
            let resize = entry.anchor(ANCHOR_RESIZE).unwrap();
            let realloc = entry.anchor(ANCHOR_REALLOC).unwrap();
            let accessor = entry.anchor(ANCHOR_ACCESSOR).unwrap();
            let ctor = entry.anchor(ANCHOR_NAME_CTOR).unwrap();
            let _ = G.set(Mutex::new(Inner {
                logger: logger.clone(),
                base,
                realloc: base + realloc.rva as usize,
                accessor: base + accessor.rva as usize,
                name_ctor: base + ctor.rva as usize,
                resize_rva: resize.rva as usize,
                resize_expect: resize.expect_bytes().unwrap_or_default(),
                resize_tramp: None,
                injected: false,
            }));
        }
        if install_hook {
            let m = G.get().unwrap();
            let mut g = m.lock().map_err(|_| "state lock poisoned".to_string())?;
            if g.resize_tramp.is_none() {
                let entry_ptr = (g.base + g.resize_rva) as *mut u8;
                let expect = g.resize_expect.clone();
                let t = patch::install_trampoline(
                    entry_ptr,
                    my_resize as *const c_void,
                    &expect,
                    sys::PAGE_EXECUTE_READWRITE,
                )?;
                let rva = g.resize_rva;
                g.logger.line(&format!(
                    "rowinject: resize hook installed rva={rva:#x} expect=verified \
                     (inject after spec_table_resize)"
                ));
                g.resize_tramp = Some(t);
            }
        }
        Ok(())
    }
}

/// The bare primitive: grow the live spec vector and append the reserved rows
/// at the CURRENT end. Called by the resize hook after the original returns, and
/// driven directly by the harness (which also uses it to show that a growth
/// before `spec_table_resize` is silently undone).
///
/// Refuses (no mutation) on any precondition failure. Returns
/// `(new_row_count, first_reserved_id, pFormName_text)`.
pub unsafe fn inject_now(logger: &log::Logger) -> Result<(usize, usize, String), String> {
    unsafe {
        ensure(false, logger)?;
        let m = G.get().unwrap();
        let g = m.lock().map_err(|_| "state lock poisoned".to_string())?;
        inject_locked(&g)
    }
}

/// The core, with the state lock held.
unsafe fn inject_locked(g: &Inner) -> Result<(usize, usize, String), String> {
    unsafe {
        let vec = (g.base + MANAGER_RVA + MGR_ROWS) as *mut u8;
        if !sys::is_readable(vec as *const c_void, 0x18) {
            return Err("the spec-vector object is unreadable".into());
        }
        let begin = *(vec as *const usize);
        let end = *((vec as *const usize).add(1));
        let cap = *((vec as *const usize).add(2));
        if begin == 0 || end < begin || cap < end {
            return Err(format!(
                "spec-vector fields nonsensical (begin={begin:#x} end={end:#x} cap={cap:#x})"
            ));
        }
        let count = (end - begin) / ROW_STRIDE;
        if count == 0 || count > 0x10000 {
            return Err(format!("spec vector has {count} rows — not the table we think it is"));
        }
        // the reserved range: u16-bounded (the save), and under the composed
        // keep-set width so the loader can actually admit its own trains.
        let first_reserved = count;
        let last_reserved = first_reserved + RESERVED_ROWS - 1;
        if last_reserved > RESERVED_MAX {
            return Err(format!(
                "reserved id {last_reserved} exceeds the u16 save-id ceiling {RESERVED_MAX}"
            ));
        }
        let forms_max = crate::forms::FORMS_MAX;
        if last_reserved >= forms_max {
            return Err(format!(
                "reserved id {last_reserved} >= the composed keep-set width \
                 (forms.rs FORMS_MAX = {forms_max}) — the loader could never admit \
                 its own train; M7-2 must widen the bitmap or fit under it"
            ));
        }
        let accessor: AccessorFn = core::mem::transmute(g.accessor);
        // the template row must be a shipped VALID train with a non-NULL name.
        let template = accessor(vec, TEMPLATE_ID);
        if template.is_null() || !sys::is_readable(template as *const c_void, ROW_STRIDE) {
            return Err(format!("template row {TEMPLATE_ID} is unreadable"));
        }
        if *template.add(ROW_BVALID) == 0 {
            return Err(format!(
                "template row {TEMPLATE_ID} is invalid (bValid == 0) — a clone must \
                 come from a shipped valid train"
            ));
        }
        if (*(template.add(ROW_PFORMNAME) as *const *const u16)).is_null() {
            return Err(format!(
                "template row {TEMPLATE_ID} has a NULL pFormName — refusing (a clone \
                 with a NULL name crashes save_train_group_restore's blank path)"
            ));
        }
        // build the row in a scratch buffer: clone, then patch the name fields
        // with fresh engine-constructed strings. A failure here refuses BEFORE
        // the vector is touched.
        let mut scratch = [0u8; ROW_STRIDE];
        core::ptr::copy_nonoverlapping(template, scratch.as_mut_ptr(), ROW_STRIDE);
        let ctor: NameCtorFn = core::mem::transmute(g.name_ctor);
        ctor(
            scratch.as_mut_ptr().add(ROW_NAME_JP) as *mut *const u16,
            JP_FMT.as_ptr(),
            first_reserved,
        );
        ctor(
            scratch.as_mut_ptr().add(ROW_NAME_EN) as *mut *const u16,
            JP_FMT.as_ptr(),
            first_reserved,
        );
        ctor(
            scratch.as_mut_ptr().add(ROW_PFORMNAME) as *mut *const u16,
            FORM_FMT.as_ptr(),
            first_reserved,
        );
        for (off, what) in [
            (ROW_NAME_JP, "JP display lane"),
            (ROW_NAME_EN, "EN display lane"),
            (ROW_PFORMNAME, "pFormName"),
        ] {
            if (*(scratch.as_ptr().add(off) as *const *const u16)).is_null() {
                return Err(format!(
                    "the name constructor produced a NULL {what} for reserved row \
                     {first_reserved} — refusing (a NULL name crashes the blank path)"
                ));
            }
        }
        let name = crate::probe::wide_str(
            *(scratch.as_ptr().add(ROW_PFORMNAME) as *const *const u16),
        )
        .unwrap_or_else(|| "<unreadable>".into());

        // grow through the ENGINE's own reallocate (its allocator, its copy,
        // its old-block free) and then append the rows.
        let target = count + RESERVED_ROWS;
        let realloc: ReallocFn = core::mem::transmute(g.realloc);
        realloc(vec, target);
        let begin2 = *(vec as *const usize);
        if begin2 == 0 {
            return Err("the engine realloc left the vector begin NULL".into());
        }
        *((vec as *const usize).add(1) as *mut usize) = begin2 + target * ROW_STRIDE;
        for i in 0..RESERVED_ROWS {
            let id = count + i;
            let row = accessor(vec, id);
            if row.is_null() || !sys::is_readable(row as *const c_void, ROW_STRIDE) {
                return Err(format!("reserved row {id} is unreadable after the growth"));
            }
            core::ptr::copy_nonoverlapping(scratch.as_ptr(), row, ROW_STRIDE);
        }
        Ok((target, first_reserved, name))
    }
}

/// The `spec_table_resize` hook: run the original (the vector re-pin), then
/// inject once. Fail-open: a refusal logs and leaves the vector as the original
/// left it.
unsafe extern "system" fn my_resize(vec: *mut u8) {
    unsafe {
        // run the original first, WITHOUT the lock held (mirrors the probes).
        let orig = G.get().and_then(|m| {
            m.lock()
                .ok()
                .and_then(|g| g.resize_tramp.as_ref().map(|t| t.as_ptr() as usize))
        });
        if let Some(p) = orig {
            let f: ResizeFn = core::mem::transmute(p);
            f(vec);
        }
        let Some(m) = G.get() else { return };
        let Ok(mut g) = m.lock() else { return };
        if g.injected {
            return;
        }
        g.injected = true;
        let logger = g.logger.clone();
        match inject_locked(&g) {
            Ok((target, id, name)) => logger.line(&format!(
                "rowinject: grew the spec vector to {target} rows through the game's \
                 realloc (game allocator); injected reserved row {id} cloned from \
                 template {TEMPLATE_ID} (pFormName {name:?})"
            )),
            Err(e) => logger.err(&format!(
                "rowinject refused: {e} — the spec vector is untouched (fail-open)"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// The harness seam. NOT wired into `engine::boot` and not read from any config
// or manifest — M7-2 owns the integration. `harness/host.c` calls this through
// GetProcAddress after it has mapped the fake manager / spec-table pages at the
// real RVAs.
//
//   op 0: verify the four anchors and install the resize hook. 0 = ok.
//   op 1: run the injection primitive at the current vector state. Returns the
//         new row count, or 0 on refusal.
//   op 2: return the current vector row count (assertion helper).
#[unsafe(no_mangle)]
pub unsafe extern "system" fn a9loader_m7_rows(op: u32) -> u64 {
    unsafe {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(std::path::Path::to_path_buf));
        let logger = match &exe_dir {
            Some(d) => log::Logger::new(&d.join("mods").join("a9loader.log")),
            None => return u64::MAX,
        };
        match op {
            0 => match ensure(true, &logger) {
                Ok(()) => 0,
                Err(e) => {
                    logger.err(&format!("rowinject refused: {e}"));
                    1
                }
            },
            1 => match inject_now(&logger) {
                Ok((target, id, name)) => {
                    logger.line(&format!(
                        "rowinject: grew the spec vector to {target} rows through the \
                         game's realloc (game allocator); injected reserved row {id} \
                         cloned from template {TEMPLATE_ID} (pFormName {name:?})"
                    ));
                    target as u64
                }
                Err(e) => {
                    logger.err(&format!(
                        "rowinject refused: {e} — the spec vector is untouched (fail-open)"
                    ));
                    0
                }
            },
            2 => {
                let base = sys::GetModuleHandleW(std::ptr::null()) as usize;
                if base == 0 {
                    return 0;
                }
                let vec = (base + MANAGER_RVA + MGR_ROWS) as *const u8;
                if !sys::is_readable(vec as *const c_void, 0x18) {
                    return 0;
                }
                let begin = *(vec as *const usize);
                let end = *((vec as *const usize).add(1));
                if begin == 0 || end < begin {
                    return 0;
                }
                ((end - begin) / ROW_STRIDE) as u64
            }
            _ => u64::MAX,
        }
    }
}

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
/// `CString` field clone (refcount bump) — the name-pointer clone fix.
pub const ANCHOR_CLONE_REF: &str = "wstr_clone_ref";
/// The shipped FORM-name literal (donor name -> id resolution).
pub const ANCHOR_FORM_LIST: &str = "form_list";

/// `A9_SpecRow_140` fields the injection writes (notes/52 §5).
const ROW_CRUISE: usize = 0x0c;
const ROW_REVERSE: usize = 0x0e;
const ROW_FARE: usize = 0x12;
const ROW_CARC0UNT: usize = 0x38;
const ROW_SPECIAL: usize = 0x9e;
/// `awName[9]` @+0xa8 and `awNameAlt[9]` @+0xf0 — the 18 name lanes.
const ROW_NAME_BASE: usize = 0xa8;
const ROW_NAMEALT_BASE: usize = 0xf0;
const NAME_LANES: usize = 9;
const NAME_LANE_STRIDE: usize = 8;

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
}

/// Immutable install-time state for the train injection.
struct TrainState {
    logger: log::Logger,
    base: usize,
    realloc: usize,
    accessor: usize,
    name_ctor: usize,
    clone_ref: usize,
    /// one resolved (donor_id, TrainRow) per train, in allocation order
    rows: Vec<(usize, TrainRow)>,
    tramp: Option<patch::Trampoline>,
    done: bool,
}

static TG: OnceLock<Mutex<TrainState>> = OnceLock::new();

/// A wide, NUL-terminated FORMAT buffer for `row_name_ctor`: the text is the
/// format, so `%` is escaped to `%%` (no conversion specifiers are used).
fn wide_format(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = Vec::with_capacity(s.len() + 1);
    for u in s.encode_utf16() {
        if u == b'%' as u16 {
            v.push(u);
        }
        v.push(u);
    }
    v.push(0);
    v
}

/// Read the live FORM list and resolve `name` to its position (id).
unsafe fn form_id(entry: &BuildEntry, base: usize, name: &str) -> Option<usize> {
    unsafe {
        let a = entry.anchor(ANCHOR_FORM_LIST)?;
        let units = crate::handlers::read_u16s_bounded(base as *const u8, a.rva as usize, 16384)
            .ok()?;
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
) -> Result<String, String> {
    unsafe {
        if trains.is_empty() {
            return Err("no trains to inject".into());
        }
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
            rows.push((donor_id, TrainRow {
                id: t.id,
                form_name: t.form_name.clone(),
                name_jp: t.name_jp.clone(),
                name_en: t.name_en.clone(),
                donor: t.donor.clone(),
                cruise_speed: t.cruise_speed,
                reverse_speed: t.reverse_speed,
                fare: t.fare,
                cars: t.cars,
            }));
        }

        let tlb_expect = tlb.expect_bytes().unwrap_or_default();
        if tlb_expect.len() < 12 {
            return Err(format!(
                "{ANCHOR_TLB} records {} expect bytes, the hook needs >= 12",
                tlb_expect.len()
            ));
        }
        let _ = TG.set(Mutex::new(TrainState {
            logger: logger.clone(),
            base: base as usize,
            realloc: base as usize + realloc.rva as usize,
            accessor: base as usize + accessor.rva as usize,
            name_ctor: base as usize + ctor.rva as usize,
            clone_ref: base as usize + clone_ref.rva as usize,
            rows,
            tramp: None,
            done: false,
        }));
        let m = TG.get().unwrap();
        let mut g = m.lock().map_err(|_| "train state lock poisoned".to_string())?;
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
        Ok(format!(
            "train injection: train_list_builder rva={rva:#x} expect=verified \
             ({n} reserved row(s) queued; inject before the builder walks the vector)"
        ))
    }
}

/// The `train_list_builder` hook: inject once (fail-open), then run the
/// original so it walks the grown vector through the composed keep-set.
unsafe extern "system" fn my_train_list_builder(manager: *mut u8) {
    unsafe {
        // inject once, WITHOUT the lock held across the original
        {
            let Some(m) = TG.get() else { return };
            let Ok(mut g) = m.lock() else { return };
            if !g.done {
                // only inject when the vector is still exactly 421 rows AND the
                // donor row is populated (the pipeline's fill loop has run)
                match inject_trains_locked(&g) {
                    Ok(n) => {
                        g.done = true;
                        let logger = g.logger.clone();
                        logger.line(&format!(
                            "train injection: grew the spec vector by {n} row(s) and \
                             wrote the reserved rows through the game's own realloc"
                        ));
                    }
                    Err(e) => {
                        g.done = true; // do not retry every call
                        let logger = g.logger.clone();
                        logger.err(&format!(
                            "train injection refused: {e} — the spec vector is \
                             untouched (fail-open)"
                        ));
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
    let Some(exe_dir) = exe.parent() else { return 0 };
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
                    "train '{}': donor row {donor_id} ({}) is not populated yet \
                     (bValid == 0) — refusing",
                    t.form_name, t.donor
                ));
            }
            let mut s = [0u8; ROW_STRIDE];
            core::ptr::copy_nonoverlapping(donor, s.as_mut_ptr(), ROW_STRIDE);
            // 1) clone every shared name lane (refcount bump) so the row owns them
            for (base_off, lanes) in [(ROW_NAME_BASE, NAME_LANES), (ROW_NAMEALT_BASE, NAME_LANES)] {
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
            let jp = wide_format(&t.name_jp);
            let en = wide_format(&t.name_en);
            let form = wide_format(&t.form_name);
            ctor(s.as_mut_ptr().add(ROW_NAME_JP) as *mut *const u16, jp.as_ptr(), 0);
            ctor(s.as_mut_ptr().add(ROW_NAME_EN) as *mut *const u16, en.as_ptr(), 0);
            ctor(s.as_mut_ptr().add(ROW_PFORMNAME) as *mut *const u16, form.as_ptr(), 0);
            for (off, what) in [
                (ROW_NAME_JP, "JP display lane"),
                (ROW_NAME_EN, "EN display lane"),
                (ROW_PFORMNAME, "pFormName"),
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
            *(s.as_mut_ptr().add(ROW_CRUISE) as *mut u16) = t.cruise_speed;
            *(s.as_mut_ptr().add(ROW_REVERSE) as *mut u16) = t.reverse_speed.unwrap_or(0);
            *(s.as_mut_ptr().add(ROW_FARE) as *mut u16) = t.fare;
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
        Ok(n)
    }
}
