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
/// `cstring_format_2args` 0x1400372F0 — a printf-style wide-string constructor:
/// `(dest, format, first_vararg)`. Always called with a static `"%s"` so author
/// text can never be interpreted as a format string.
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
/// The preview annotator's loop bound (0x1403B0A7F, `CMP EBP,0x1a1`). Widening
/// its immediate to the live row count is what lets an injected row get a
/// rolling-stock list icon at all: the annotator composes the preview names for
/// every valid row it walks, and the shipped literal stops at 417.
pub const ANCHOR_ANNOT_BOUND: &str = "preview_annotator_bound";
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

/// `A9_SpecRow_140` fields the injection writes (notes/52 §5).
const ROW_CRUISE: usize = 0x0c;
const ROW_REVERSE: usize = 0x0e;
const ROW_ACCEL: usize = 0x10;
const ROW_FARE: usize = 0x12;
const ROW_SOUND_START: usize = 0x1c;
const ROW_SOUND_DRIVE: usize = 0x1e;
const ROW_SOUND_HORN: usize = 0x20;
const ROW_SOUND_BRAKE: usize = 0x22;
const ROW_PREVIEW_X: usize = 0x24;
const ROW_PREVIEW_Y: usize = 0x26;
const ROW_PREVIEW2D_X: usize = 0x28;
const ROW_PREVIEW2D_Y: usize = 0x2a;
const ROW_SPECIAL_FLAG: usize = 0x9e;
const ROW_MODE_HORN: usize = 0x9f;
const ROW_MODE_ACCEL: usize = 0xa0;
const ROW_CARC0UNT: usize = 0x38;
/// The consist length the purchase panel prints as "Train Configuration".
/// Live evidence (2026-10-15): AR4's row carries 4 here and the panel said
/// "Train Configuration 4", while the row's car-number fields (+0x14/+0x16/+0x18)
/// and carCount (+0x38) carry its *modelled* cars (2). A cloned row therefore
/// keeps the donor's consist length, so the car build must write it.
const ROW_CONSIST_LEN: usize = 0x1a;

const ROW_SPECIAL: usize = 0x9e;
const ROW_MODEL_SLOT: usize = 0x2c;
const ROW_CAR_BLOCK: usize = 0x39;
const ROW_FIRST_CAR: usize = 0x14;
const ROW_LAST_CAR: usize = 0x16;
const ROW_STD_CAR: usize = 0x18;
const ROW_CAR_FLAGS: usize = 0x9d;
/// The wrapper record the game's car-model loader is handed:
/// `{+0x00 obj, +0x08 capacity u32, +0x0c running cost u32, +0x10 price u64}`.
/// The three numbers are the pattern row's CSV columns 3/4/5 — what the pipeline
/// copies in before the call, and what the shop aggregates per car.
const WRAP_CAPACITY: usize = 0x08;
const WRAP_RUNNING_COST: usize = 0x0c;
const WRAP_PRICE: usize = 0x10;
// The shop's figures are the SUM OVER THE CONSIST'S CARS: each car contributes
// its own `train_pattern` values, and the consist's car *types* come from the
// pattern group's coupling rows (row N of a group = the trailing cars' types for
// an N-car consist, -1-terminated; no coupling = every car is the lead's type).
// Live proof (user-driven, 2026-10-15): AR4 with 4 cars reads 708 / $20,160,000 =
// 2x type-1 (168 / 528,000,000) + 2x type-2 (186 / 480,000,000); with 8 cars it
// reads 1452 / $39,360,000 = 2x type-1 + 6x type-2 — slightly more than double
// the capacity and slightly less than double the price, exactly as a mixed
// consist should. A single-car train doubles when doubled. So a `[[train]]`
// price / capacity / running cost is the CAR's value in shipped-CSV units, and
// the loader stores it 1:1; our one-car consist then reads its own car's numbers.
// (An earlier round doubled these values on a misreading of AR4's 4-car case.)
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
}

/// Immutable install-time state for the train injection.
struct TrainState {
    logger: log::Logger,
    base: usize,
    realloc: usize,
    accessor: usize,
    name_ctor: usize,
    clone_ref: usize,
    /// the preview annotator's bound site (optional: a missing anchor or a byte
    /// mismatch only costs the injected rows their list icon)
    annot_bound: Option<u32>,
    /// the car build's resolved addresses (all optional: without them the
    /// injected rows keep the donor's car data, as before). All four are CALLED,
    /// never hooked.
    model_fetch: Option<usize>,
    model_register: Option<usize>,
    model_index_alloc: Option<usize>,
    rcstr_ctor: Option<usize>,
    /// opt-in: drive the game's car-model loader (a9loader.toml `car_build`)
    car_build: bool,
    /// the car build waits for a LATER builder call: later calls come after the
    /// boot (e.g. when a scenario is loaded), when no pack load is in flight.
    car_built: bool,

    /// one resolved (donor_id, TrainRow) per train, in allocation order
    rows: Vec<(usize, TrainRow)>,
    tramp: Option<patch::Trampoline>,
    done: bool,
    /// how many `train_list_builder` calls have been seen, and whether the
    /// "not ready yet" warning has been emitted (once per boot)
    attempts: u32,
    warned: bool,
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
    cfg_car_build: bool,
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
                model_dir: t.model_dir.clone(),
                model_mesh: t.model_mesh.clone(),
                capacity: t.capacity,
                running_cost: t.running_cost,
                price: t.price,
                form_cols: t.form_cols.clone(),
            }));
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
            model_fetch,
            model_register,
            model_index_alloc,
            rcstr_ctor,
            car_build: cfg_car_build,
            car_built: false,
            rows,
            tramp: None,
            done: false,
            attempts: 0,
            warned: false,
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

/// DIAGNOSTIC: dump what the loader can see of the spec table at each
/// `train_list_builder` call.
///
/// The live acceptance boot refused twice with "donor row 195 (FORM_KH32) is not
/// populated yet (bValid == 0)", including at a call that came AFTER the boot had
/// finished — so either the row index we look up is not the row the game fills,
/// or the fill happens somewhere we never observe. This prints the vector's real
/// extent, the validity byte at a few known indices, the first populated index,
/// the name strings at 195 and 212, and where (if anywhere) a row actually calls
/// itself FORM_KH32. One boot with this in the log settles it.
unsafe fn probe_row_map(g: &TrainState, call: u32, mgr: *mut u8) {
    unsafe {
        let ours = g.base + MANAGER_RVA;
        let vec = (ours + MGR_ROWS) as *const usize;
        let (begin, end) = if sys::is_readable(vec as *const c_void, 0x18) {
            (*vec, *vec.add(1))
        } else {
            (0, 0)
        };
        let count = if begin != 0 && end >= begin { (end - begin) / ROW_STRIDE } else { 0 };

        let name_at = |idx: usize| -> String {
            if idx >= count {
                return "<oob>".into();
            }
            let row = (begin + idx * ROW_STRIDE) as *const u8;
            if !sys::is_readable(row as *const c_void, ROW_STRIDE) {
                return "<unreadable>".into();
            }
            let p = *(row.add(ROW_PFORMNAME) as *const *const u16);
            if p.is_null() || !sys::is_readable(p as *const c_void, 2) {
                return "<null>".into();
            }
            let mut s = String::new();
            for i in 0..24 {
                let c = *p.add(i);
                if c == 0 {
                    break;
                }
                s.push(char::from_u32(c as u32).unwrap_or('?'));
            }
            s
        };
        // DIAGNOSTIC: the 18 name lanes of a row (9 at +0xa8, 9 at +0xf0). The
        // panel's price/capacity/description come from the CSV tables, whose keys
        // are UPPERCASE FORM names, and the injected row's pFormName is lowercase
        // — so whatever name the lookup uses is one of these lanes, and after the
        // clone the unpatched ones still hold the DONOR's names. Dumping them for
        // the donor and the injected row shows exactly which lane has to change.
        let lanes_at = |idx: usize| -> String {
            if idx >= count {
                return "<oob>".into();
            }
            let row = (begin + idx * ROW_STRIDE) as *const u8;
            if !sys::is_readable(row as *const c_void, ROW_STRIDE) {
                return "<unreadable>".into();
            }
            let mut out = Vec::new();
            for (base_off, tag) in [(ROW_NAME_BASE, "n"), (ROW_NAMEALT_BASE, "a")] {
                for k in 0..9 {
                    let off = base_off + k * 0x8;
                    if off + 8 > ROW_STRIDE {
                        break;
                    }
                    let p = *(row.add(off) as *const *const u16);
                    if p.is_null() || !sys::is_readable(p as *const c_void, 2) {
                        out.push(format!("{tag}{k}=<null>"));
                        continue;
                    }
                    let mut s = String::new();
                    for i in 0..20 {
                        let c = *p.add(i);
                        if c == 0 {
                            break;
                        }
                        s.push(char::from_u32(c as u32).unwrap_or('?'));
                    }
                    out.push(format!("{tag}{k}={s}"));
                }
            }
            out.join(" ")
        };
        let b0 = |idx: usize| -> String {
            if idx >= count {
                return "<oob>".into();
            }
            let row = (begin + idx * ROW_STRIDE) as *const u8;
            if !sys::is_readable(row as *const c_void, 1) {
                return "?".into();
            }
            (*row).to_string()
        };
        let first = (0..count)
            .find(|&i| b0(i) == "1")
            .map(|i| i.to_string())
            .unwrap_or_else(|| "none".into());
        let lower = |s: &str| s.to_ascii_lowercase();
        let populated: Vec<usize> = (0..count).filter(|&i| b0(i) == "1").collect();
        let sample: Vec<String> = populated
            .iter()
            .take(12)
            .map(|&i| format!("{i}:{}", name_at(i)))
            .collect();
        let cand = |want: &str| -> String {
            (0..count)
                .find(|&i| lower(&name_at(i)) == lower(want))
                .map(|i| {
                    if b0(i) == "1" {
                        format!("{i}:valid")
                    } else {
                        format!("{i}:INVALID")
                    }
                })
                .unwrap_or_else(|| "absent".into())
        };

        g.logger.debug(&format!(
            "train probe: call {call} mgr={mgr:p} ours={ours:#x} count={count} \
             populated={} b0[0]={} b0[62]={} b0[195]={} b0[212]={} first_valid={first} \
             name[195]={:?} name[212]={:?} FORM_KH32_at={} | AR4={} 285={} 201KY={} \
             sample=[{}]",
            populated.len(),
            b0(0),
            b0(62),
            b0(195),
            b0(212),
            name_at(195),
            name_at(212),
            (0..count)
                .find(|&i| lower(&name_at(i)) == "form_kh32")
                .map(|i| i.to_string())
                .unwrap_or_else(|| "none".into()),
            cand("FORM_AR4"),
            cand("FORM_285"),
            cand("FORM_201KY"),
            sample.join(", ")
        ));
        if count > 0 {
            g.logger.debug(&format!(
                "train probe lanes: donor(96) [{}]",
                lanes_at(96)
            ));
            if count > 421 {
                g.logger.debug(&format!(
                    "train probe lanes: injected(421) [{}]",
                    lanes_at(421)
                ));
            }
            // DIAGNOSTIC: the raw first 0x60 bytes of the donor row and of the
            // injected row. The panel's price/capacity/configuration are the train's
            // pattern-group aggregates (the group head for FORM_AR4 is at data row
            // 2322 with capacity 168 / price 528,000,000 — neither of which is what
            // the panel showed), so the row must still reference the DONOR's group
            // somewhere in these bytes. A field-by-field diff pins the offset.
            let hex_at = |idx: usize| -> String {
                if idx >= count {
                    return "<oob>".into();
                }
                let row = (begin + idx * ROW_STRIDE) as *const u8;
                if !sys::is_readable(row as *const c_void, ROW_STRIDE) {
                    return "<unreadable>".into();
                }
                let mut s = String::new();
                for i in 0..0x60 {
                    s.push_str(&format!("{:02x}", *row.add(i)));
                }
                s
            };
            g.logger
                .debug(&format!("train probe bytes: donor(96) {}", hex_at(96)));
            if count > 421 {
                g.logger
                    .debug(&format!("train probe bytes: injected(421) {}", hex_at(421)));
            }
        }
    }
}

/// The game's own car-model loader, `train_model_fetch` 0x19B9F0:
/// `(wrapper, &mesh_path_cell, pattern_row, flag) -> u8` (success in AL). The
/// object it creates is stored into `wrapper[0]`.
type ModelFetchFn = unsafe extern "system" fn(*mut u8, *const *const u16, *const u8, u8) -> u8;
type ModelRegisterFn = unsafe extern "system" fn(*const u8, *const u8);
type ModelIndexFn = unsafe extern "system" fn(*const u8) -> u16;
/// The printf-style wide-string constructor the pipeline uses to compose the
/// mesh path (0x1400372F0, the same function the name lanes use):
/// `(dest, format, arg)`.
type CellCtorFn = unsafe extern "system" fn(*mut u8, *const u16, usize) -> *mut u8;
/// `(dest)` — constructs an empty string object at dest.
type RcStrCtorFn = unsafe extern "system" fn(*mut u8);

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
            probe_row_map(&g, g.attempts, manager);
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
/// The car build: drive the game's own car-model loader once per declared car, so
/// an injected row gets the car data the CSV pipeline can never give it (that pass
/// resolves rows through the 417-entry FORM arrays and runs before our hook).
///
/// This is the SAME call the game's own pattern loop makes for an ordinary car
/// (`train_csv_pipeline_builder` 0x14019cb20..0x14019cbe9):
///
/// ```text
///   train_model_fetch(wrapper, &mesh_path_cell, pattern_row, flag)   // 0x19B9F0
///   if (wrapper[0]) { model_register(manager, wrapper); idx = model_index_alloc(manager); }
///   row->slot[car] = idx - 1;  row->carCount++;
/// ```
///
/// `wrapper` is the record the pipeline hands the loader:
/// One `train_form.csv` cell as a number (the vanilla cells are plain integers;
/// the loader never guesses at a value it cannot read).
fn cell_u16(cols: &[String], idx: usize) -> Option<u16> {
    cols.get(idx)?.trim().parse::<u16>().ok()
}

/// Apply a train's `train_form` row — the CSV columns in the vanilla layout — to
/// the cloned spec row. Column → field map from the RE agent's Ghidra pass
/// (corpus notes/52 §7, instruction-level):
///
/// | col | header | field |
/// |---|---|---|
/// | 4 | 巡航速度 | `wCruiseSpeed` +0x0c |
/// | 5 | 反転速度 | `wReverseSpeed` +0x0e |
/// | 6 | 加速 | `wAccel` +0x10 |
/// | 7 | 基本運賃 | `wBaseFare` +0x12 |
/// | 8..11 | 発進/駆動/警笛/ブレーキ音ID | +0x1c / +0x1e / +0x20 / +0x22 |
/// | 12..15 | preview x/y, 2D preview x/y | +0x24 / +0x26 / +0x28 / +0x2a |
/// | 16 | 特殊車両フラグ | `bSpecialFlag` +0x9e |
/// | 17 / 18 | 運転モード警笛 / 加減速 | +0x9f / +0xa0 |
///
/// The game resolves cols 2/3 (車両種別ID / 鉄道会社ID) and 8..11, 17, 18 through
/// name tables, so only a *numeric* cell is taken here; anything else keeps the
/// donor's value instead of writing a wrong id (fail open). Column 1 (有効) is
/// forced to valid: a row the loader materialises is always meant to be live, and
/// a mod disables a train through its own `enabled` flag, not through this cell.
unsafe fn apply_form_row(row: *mut u8, cols: &[String]) {
    let set16 = |off: usize, v: Option<u16>| {
        if let Some(v) = v {
            unsafe { core::ptr::write_unaligned(row.add(off) as *mut u16, v) };
        }
    };
    let set8 = |off: usize, v: Option<u16>| {
        if let Some(v) = v {
            unsafe { *row.add(off) = v as u8 };
        }
    };
    set16(ROW_CRUISE, cell_u16(cols, 4));
    set16(ROW_REVERSE, cell_u16(cols, 5));
    set16(ROW_ACCEL, cell_u16(cols, 6));
    set16(ROW_FARE, cell_u16(cols, 7));
    set16(ROW_SOUND_START, cell_u16(cols, 8));
    set16(ROW_SOUND_DRIVE, cell_u16(cols, 9));
    set16(ROW_SOUND_HORN, cell_u16(cols, 10));
    set16(ROW_SOUND_BRAKE, cell_u16(cols, 11));
    set16(ROW_PREVIEW_X, cell_u16(cols, 12));
    set16(ROW_PREVIEW_Y, cell_u16(cols, 13));
    set16(ROW_PREVIEW2D_X, cell_u16(cols, 14));
    set16(ROW_PREVIEW2D_Y, cell_u16(cols, 15));
    set8(ROW_SPECIAL_FLAG, cell_u16(cols, 16));
    set8(ROW_MODE_HORN, cell_u16(cols, 17));
    set8(ROW_MODE_ACCEL, cell_u16(cols, 18));
    unsafe { *row = 1 }; // bValid
}

/// `{+0x00 obj, +0x08 capacity u32, +0x0c running cost u32, +0x10 price u64}` —
/// those three numbers are the pattern row's CSV columns 3/4/5, i.e. the same
/// per-car figures the shop's 定員積載 / 運行費用 / 購入価格 display aggregates
/// over the formation. `pattern_row` is the 17-cell row (key, dir, mesh, the three
/// numbers, ten block bytes, the standard flag); `flag` selects CModelCUTTrain over
/// CModelTrain and is 0 for every id a mod can allocate.
///
/// Cells are built exactly the way the pipeline builds them: `rcstr_ctor(dest)`
/// first, because both the empty-string constructor AND the printf-style
/// constructor 0x1400372F0 ASSIGN into an already-live A9_RcStr — calling either
/// on a zeroed buffer dereferences NULL (that was crash #1 and #2 of this build).
/// The format passed to the printf-style constructor is always a static `"%s"`, so
/// author text can never be read as a format string.
///
/// The row's 100-byte car/formation block at `+0x39` is deliberately NOT written
/// here: the game fills it from the group's *formation* columns, and this build logs
/// the clone's bytes instead so the next boot's evidence settles their meaning
/// (one theory read them as a 10x10 formation matrix, the donor's bytes disagree).
///
/// Fail-open: any missing anchor or a refused load leaves the row exactly as the
/// clone left it (the donor's car data) — the behaviour before this build.
/// Car-build scratch: the live bytes each buffer needs, plus a canary tail that is
/// checked after the game's loader returns.
const CAR_ROW_BYTES: usize = 0x88; // 17 pattern-row cells, 8 bytes each
const CAR_WRAP_BYTES: usize = 0x38; // the pipeline's own wrapper local is `[rbp-0x38]`
const CAR_CELL_BYTES: usize = 8; // one A9_RcStr pointer slot (the mesh-path cell)
const CAR_CANARY: usize = 0x20;
const CAR_CANARY_FILL: u8 = 0xA5;

/// A leaked scratch buffer: zeroed for the live bytes, `CAR_CANARY_FILL` after them.
/// Leaked on purpose — the game's loader is handed pointers into these buffers, so a
/// game-side write past one of them must land in our own allocation instead of this
/// function's frame (an unexplained spurious loop iteration came from exactly that),
/// and nothing may dangle if the loader keeps a pointer past the call.
fn new_scratch(live: usize) -> &'static mut [u8] {
    let mut v = vec![0u8; live + CAR_CANARY].into_boxed_slice();
    for b in &mut v[live..] {
        *b = CAR_CANARY_FILL;
    }
    Box::leak(v)
}

/// True while `base[live..total]` still holds the untouched canary.
fn canary_ok(base: *const u8, live: usize, total: usize) -> bool {
    (live..total).all(|k| unsafe { *base.add(k) } == CAR_CANARY_FILL)
}

unsafe fn build_cars(g: &TrainState) -> Result<usize, String> {
    unsafe {
        let (Some(fetch_addr), Some(register), Some(index_alloc), Some(rcstr)) = (
            g.model_fetch,
            g.model_register,
            g.model_index_alloc,
            g.rcstr_ctor,
        ) else {
            return Err("the car-build helper anchors are not all verified (see the boot \
                        warnings)"
                .into());
        };
        let manager = (g.base + MANAGER_RVA) as *const u8;
        let vec = (g.base + MANAGER_RVA + MGR_ROWS) as *mut u8;
        let accessor: AccessorFn = core::mem::transmute(g.accessor);
        let fetch: ModelFetchFn = core::mem::transmute(fetch_addr);
        let register_fn: ModelRegisterFn = core::mem::transmute(register);
        let index_fn: ModelIndexFn = core::mem::transmute(index_alloc);
        let cell_ctor: CellCtorFn = core::mem::transmute(g.name_ctor);
        let rcstr_ctor: RcStrCtorFn = core::mem::transmute(rcstr);

        let wide = |s: &str| -> Vec<u16> {
            let mut v: Vec<u16> = s.encode_utf16().collect();
            v.push(0);
            v
        };
        // the object a car is built from is freed through its vtable's first slot,
        // with `1` (scalar deleting destructor) — what the pipeline does on failure.
        unsafe fn destroy_car(obj: *mut u8) {
            unsafe {
                let vt = *(obj as *const *const usize);
                let dtor: unsafe extern "system" fn(*mut u8, u32) =
                    core::mem::transmute(*vt as *const ());
                dtor(obj, 1);
            }
        }

        let fmt = wide("%s");
        let mut built = 0usize;
        // The row list is immutable for the duration of the build, so the count is
        // taken ONCE and indexed with `get` (never a slice iterator whose bounds the
        // codegen may reload after the raw game calls). `intact` then re-checks the
        // list's *shape* — lengths only, no dereferences — around every raw game call:
        // anything that moves those fields is refused loudly instead of feeding the
        // loader a garbage row (which is what a spurious second iteration did).
        let nrows = g.rows.len();
        let shape = g
            .rows
            .first()
            .map(|(_, t)| (t.model_dir.len(), t.form_name.len(), t.model_mesh.len()));
        let intact = |step: &str| -> Result<(), String> {
            let now = g
                .rows
                .first()
                .map(|(_, t)| (t.model_dir.len(), t.form_name.len(), t.model_mesh.len()));
            if g.rows.len() == nrows && now == shape {
                Ok(())
            } else {
                Err(format!(
                    "the train list changed shape during the car build ({step}: {} rows now, \
                     {nrows} before) — refusing, the reserved row keeps the donor's car data",
                    g.rows.len()
                ))
            }
        };
        for i in 0..nrows {
            let Some((_, t)) = g.rows.get(i) else { break };
            // This version authors one car per train (the compiler refuses cars > 1).
            let path = format!("train\\{}\\{}.x", t.model_dir, t.model_mesh);
            let mut cells: Vec<String> = vec![
                t.form_name.clone(),
                t.model_dir.clone(),
                t.model_mesh.clone(),
                t.capacity.to_string(),
                t.running_cost.to_string(),
                t.price.to_string(),
            ];
            // the block columns: the head car of the formation, then the trailing
            // cells (one car -> nothing to couple). The loader reads 3/4/5 only.
            cells.push("1".to_string());
            for _ in 1..10 {
                cells.push(String::new());
            }
            cells.push("1".to_string()); // the standard car

            g.logger.debug(&format!(
                "car build: constructing cells for '{}' (mesh {path})",
                t.form_name
            ));
            let row = new_scratch(CAR_ROW_BYTES);
            let wrapper = new_scratch(CAR_WRAP_BYTES);
            let path_cell = new_scratch(CAR_CELL_BYTES);
            let mut sources: Vec<Vec<u16>> = Vec::with_capacity(cells.len() + 1);
            for (k, c) in cells.iter().enumerate() {
                let w = wide(c);
                let field = row.as_mut_ptr().add(k * 8);
                rcstr_ctor(field);
                let out = cell_ctor(field, fmt.as_ptr(), w.as_ptr() as usize);
                if out.is_null() {
                    return Err(format!("the cell constructor refused cell {k} ({c:?})"));
                }
                sources.push(w);
            }
            intact("after the cell construction")?;
            let path_w = wide(&path);
            rcstr_ctor(path_cell.as_mut_ptr());
            let path_cell_ok = cell_ctor(path_cell.as_mut_ptr(), fmt.as_ptr(), path_w.as_ptr() as usize);
            sources.push(path_w);
            if path_cell_ok.is_null() {
                return Err(format!("the cell constructor refused the mesh path {path:?}"));
            }
            g.logger.debug(&format!(
                "car build: cells built for '{}' (mesh {})",
                t.form_name, path
            ));

            // The wrapper is OURS: capacity / running cost / price are the numbers
            // the loader publishes per car, and the shop's panel aggregates them.
            // one car, so its own values: the consist sum is ours alone
            *(wrapper.as_mut_ptr().add(WRAP_CAPACITY) as *mut u32) = t.capacity as u32;
            *(wrapper.as_mut_ptr().add(WRAP_RUNNING_COST) as *mut u32) = t.running_cost as u32;
            *(wrapper.as_mut_ptr().add(WRAP_PRICE) as *mut u64) = t.price;
            g.logger.debug(&format!(
                "car build: calling the game's car-model loader ({path}; capacity {}, running \
                 cost {}, price {})",
                t.capacity, t.running_cost, t.price
            ));
            let ok = fetch(
                wrapper.as_mut_ptr(),
                path_cell.as_ptr() as *const *const u16,
                row.as_ptr(),
                0,
            );
            intact("after the model loader returned")?;
            let obj = *(wrapper.as_ptr() as *const *mut u8);
            g.logger.debug(&format!(
                "car build: the loader returned {ok} (object {obj:p}) for {path}"
            ));
            // Canary check: the loader is handed pointers into all three buffers. If it
            // wrote past one, say so loudly instead of letting the overrun surface later
            // as something inexplicable (the buffers are ours and leaked, so nothing
            // else is affected either way).
            for (what, base, live) in [
                ("pattern row", row.as_ptr(), CAR_ROW_BYTES),
                ("wrapper", wrapper.as_ptr(), CAR_WRAP_BYTES),
                ("mesh-path cell", path_cell.as_ptr(), CAR_CELL_BYTES),
            ] {
                if !canary_ok(base, live, live + CAR_CANARY) {
                    g.logger.warn(&format!(
                        "car build: the game's car-model loader wrote past our {what} buffer \
                         for {path} — the scratch buffers are loader-owned, so nothing else \
                         is affected"
                    ));
                }
            }
            if ok == 0 {
                if !obj.is_null() {
                    destroy_car(obj);
                }
                g.logger.warn(&format!(
                    "car build: the game's loader refused {} (train '{}') — the row keeps the \
                     donor's car data",
                    path, t.form_name
                ));
                continue;
            }
            register_fn(manager, wrapper.as_ptr());
            let idx = index_fn(manager);
            intact("after registering the car model")?;
            g.logger.debug(&format!(
                "car build: registered the car and took model index {idx} for {path}"
            ));

            let row_ptr = accessor(vec, t.id);
            if row_ptr.is_null() || !sys::is_readable(row_ptr as *const c_void, ROW_STRIDE) {
                return Err(format!("reserved row {} is unreadable for the car build", t.id));
            }
            let donor_block: Vec<u8> =
                (0..0x10).map(|k| *row_ptr.add(ROW_CAR_BLOCK + k)).collect();
            let donor_carcount = *row_ptr.add(ROW_CARC0UNT);
            *(row_ptr.add(ROW_MODEL_SLOT) as *mut u16) = idx.wrapping_sub(1);
            // The purchase panel walks the row's model slots and skips the ones whose
            // index is out of range — shipped rows leave uninitialised slots holding
            // heap junk, which is skipped exactly that way. The donor's *valid*
            // leftover entries are therefore what the panel sums on top of our car
            // (live: AR4's second car in slot 1 = index 208 added 186 capacity and
            // 480,000,000 price). Park the remaining slots on an out-of-range value so
            // the game's own range check skips them; a small value like 0 or 7 is a
            // *valid* model index, so the panel would count an extra model.
            for k in 1..6 {
                *(row_ptr.add(ROW_MODEL_SLOT + k * 2) as *mut u16) = u16::MAX;
            }
            *row_ptr.add(ROW_CARC0UNT) = 1;
            *(row_ptr.add(ROW_CONSIST_LEN) as *mut u16) = 1;
            *(row_ptr.add(ROW_FIRST_CAR) as *mut u16) = 1;
            *(row_ptr.add(ROW_LAST_CAR) as *mut u16) = 1;
            *(row_ptr.add(ROW_STD_CAR) as *mut u16) = 1;
            *row_ptr.add(ROW_CAR_FLAGS) = 1;
            // The per-car block (+0x39, ten bytes per car) is what the game's consist
            // builder reads to build a train object's car entries: the first byte of a
            // car's ten is "this car exists", the rest is its coupling data, filled by
            // the pipeline from the pattern row's columns 6..15 (the compiler writes
            // "1" into column 6 for its one-car template). A cloned row inherits the
            // donor's block, and our clone was taken mid-pipeline while the donor's was
            // still zero — live 2026-10-15 that made the built car entry carry slot 0
            // and selecting the train crashed in `train_row_car_wiring`
            // (0x1401aba4e: `aModelIdx[slot - 1]` with slot 0 walks a wild index).
            // A one-car consist therefore needs car 1's first byte set and the rest
            // clear, exactly as the derived pattern row describes.
            for k in 0..0x64 {
                *row_ptr.add(ROW_CAR_BLOCK + k) = 0;
            }
            *row_ptr.add(ROW_CAR_BLOCK) = 1;
            let our_block: Vec<u8> =
                (0..0x10).map(|k| *row_ptr.add(ROW_CAR_BLOCK + k)).collect();
            g.logger.line(&format!(
                "car build: row {} now has 1 car (model index {}); the per-car block at \
                 +0x39 was {donor_block:02x?} (donor carCount {donor_carcount}), now \
                 {our_block:02x?}",
                t.id,
                idx.wrapping_sub(1)
            ));
            built += 1;
        }
        Ok(built)
    }
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
            let form: Vec<u16> = t.form_name.to_lowercase().encode_utf16().chain([0]).collect();
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

        Ok(n)
    }
}
