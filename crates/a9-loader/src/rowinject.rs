//! M7-1 — **direct spec-row injection** (the mechanism; deliberately NOT wired
//! into the boot engine or the manifest — M7-2 does that). Corpus notes/57 §6.2
//! (architecture Q), notes/62 §1/§4, PLAN §M7.
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
//! * The clone shares the template's *other* name pointers (`awName[2..8]`,
//!   `awNameAlt[0..8]`) without a refcount increment; the LIVE teardown could
//!   then double-free them. M7-2 must clone them through `wstr_clone_data`
//!   0x14001d800 (or set the unused lanes) before this is wired in. Not
//!   exercised here (M7-1 is not boot-wired).
//! * `install` / `inject_now` are driven from the harness through the
//!   [`a9loader_m7_rows`] export; nothing in `engine::boot` calls this module.

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
