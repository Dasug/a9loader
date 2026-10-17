//! the direct spec-row injection: the resolved engine state, the
//! reserve primitive and the `spec_table_resize` hook. This is the harness
//! seam (`a9loader_m7_rows`); see the parent module's doc for the mechanism,
//! `train.rs` for the `[[train]]` path that builds on it.

use crate::{log, patch, sys};
use a9_mod::builds::BuildEntry;
use core::ffi::c_void;
use std::sync::{Mutex, OnceLock};

/// `std::vector<A9_SpecRow_140>::resize(421)` — the boot-time re-pin.
pub const ANCHOR_RESIZE: &str = "spec_table_resize";
/// The vector's own reallocate (`(&vec, new_total_rows)`, game allocator).
pub(super) const ANCHOR_REALLOC: &str = "spec_table_realloc";
/// `(rcx = &vec, rdx = id) -> row*` = `*(vec) + id*0x140`.
pub(super) const ANCHOR_ACCESSOR: &str = "spec_table_row_accessor";
/// The builder's formatted-name constructor (`&field, L"form_id_%d", id`).
pub(super) const ANCHOR_NAME_CTOR: &str = "row_name_ctor";

/// DIAGNOSTIC anchor: the consist row copier (see `my_consist_copy`).
pub(super) const ANCHOR_CONSIST_COPY: &str = "consist_row_copy";

/// DIAGNOSTIC anchor: the consist object creator (see `my_consist_create`).
pub(super) const ANCHOR_CONSIST_CREATE: &str = "consist_object_create";

/// The spec-table manager singleton (0x140e14510).
pub(super) const MANAGER_RVA: usize = 0xE14510;
/// The manager's rows vector object `{begin,end,cap}` (`manager+0x18`).
pub(super) const MGR_ROWS: usize = 0x18;
/// The spec-row stride.
pub(super) const ROW_STRIDE: usize = 0x140;
/// `A9_SpecRow_140` fields.
pub(super) const ROW_BVALID: usize = 0x00;
pub(super) const ROW_NAME_JP: usize = 0xa8;
pub(super) const ROW_NAME_EN: usize = 0xb0;
pub(super) const ROW_PFORMNAME: usize = 0x138;
/// The per-car save id is a u16 — the hard upper bound.
const RESERVED_MAX: usize = 65535;

/// The shipped, valid template row this mechanism clones (harness: row 0).
const TEMPLATE_ID: usize = 0;
/// How many reserved rows the mechanism injects (one).
const RESERVED_ROWS: usize = 1;

/// `L"form_id_%d"` — the engine's own auto-name shape (its literal lives at
/// 0x14070bd88; the text is identical, so the loader keeps its own copy and no
/// game .rdata page is needed).
static FORM_FMT: [u16; 11] = [
    0x66, 0x6f, 0x72, 0x6d, 0x5f, 0x69, 0x64, 0x5f, 0x25, 0x64, 0,
];
/// `L"modrow_%d"` — the injected row's JP/EN display lanes (stand-in text).
static JP_FMT: [u16; 10] = [0x6d, 0x6f, 0x64, 0x72, 0x6f, 0x77, 0x5f, 0x25, 0x64, 0];

type ResizeFn = unsafe extern "system" fn(*mut u8);
pub(super) type ReallocFn = unsafe extern "system" fn(*mut u8, usize);
pub(super) type AccessorFn = unsafe extern "system" fn(*const u8, usize) -> *mut u8;
/// `cstring_format_2args` 0x1400372F0 — a printf-style wide-string constructor:
/// `(dest, format, first_vararg)`. Always called with a static `"%s"` so author
/// text can never be interpreted as a format string.
pub(super) type NameCtorFn = unsafe extern "system" fn(*mut *const u16, *const u16, usize);

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
fn anchor_bytes(
    entry: &BuildEntry,
    base: usize,
    name: &str,
    len: usize,
) -> Result<Vec<u8>, String> {
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
pub(super) fn verify_anchor(entry: &BuildEntry, base: usize, name: &str) -> Result<(), String> {
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
            for name in [
                ANCHOR_RESIZE,
                ANCHOR_REALLOC,
                ANCHOR_ACCESSOR,
                ANCHOR_NAME_CTOR,
            ] {
                verify_anchor(&entry, base, name)?;
            }
            let resize = entry.anchor(ANCHOR_RESIZE).unwrap();
            let realloc = entry.anchor(ANCHOR_REALLOC).unwrap();
            let accessor = entry.anchor(ANCHOR_ACCESSOR).unwrap();
            let ctor = entry.anchor(ANCHOR_NAME_CTOR).unwrap();
            // DIAGNOSTIC: the consist copier, so the {form, aux} pairs that reach
            // train_row_car_wiring are visible for vanilla trains and ours alike.
            // the consist diagnostic is installed by `install_trains` (the live path)
            // and gated on `probe_consist`; this export is the harness's entry only.
            let _ = install_hook;
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
/// `A9_SpecRow_140` fields the injection writes.
pub(super) const ROW_CRUISE: usize = 0x0c;
pub(super) const ROW_REVERSE: usize = 0x0e;
pub(super) const ROW_ACCEL: usize = 0x10;
pub(super) const ROW_FARE: usize = 0x12;
pub(super) const ROW_SOUND_START: usize = 0x1c;
pub(super) const ROW_SOUND_DRIVE: usize = 0x1e;
pub(super) const ROW_SOUND_HORN: usize = 0x20;
pub(super) const ROW_SOUND_BRAKE: usize = 0x22;
pub(super) const ROW_PREVIEW_X: usize = 0x24;
pub(super) const ROW_PREVIEW_Y: usize = 0x26;
pub(super) const ROW_PREVIEW2D_X: usize = 0x28;
pub(super) const ROW_PREVIEW2D_Y: usize = 0x2a;
pub(super) const ROW_SPECIAL_FLAG: usize = 0x9e;
pub(super) const ROW_MODE_HORN: usize = 0x9f;
pub(super) const ROW_MODE_ACCEL: usize = 0xa0;
pub(super) const ROW_CARC0UNT: usize = 0x38;
/// The consist length the purchase panel prints as "Train Configuration".
/// Live evidence (2026-10-15): AR4's row carries 4 here and the panel said
/// "Train Configuration 4", while the row's car-number fields (+0x14/+0x16/+0x18)
/// and carCount (+0x38) carry its *modelled* cars (2). A cloned row therefore
/// keeps the donor's consist length, so the car build must write it.
pub(super) const ROW_CONSIST_LEN: usize = 0x1a;

pub(super) const ROW_SPECIAL: usize = 0x9e;
pub(super) const ROW_MODEL_SLOT: usize = 0x2c;
pub(super) const ROW_CAR_BLOCK: usize = 0x39;
pub(super) const ROW_FIRST_CAR: usize = 0x14;
pub(super) const ROW_LAST_CAR: usize = 0x16;
pub(super) const ROW_STD_CAR: usize = 0x18;
pub(super) const ROW_CAR_FLAGS: usize = 0x9d;
/// The wrapper record the game's car-model loader is handed:
/// `{+0x00 obj, +0x08 capacity u32, +0x0c running cost u32, +0x10 price u64}`.
/// The three numbers are the pattern row's CSV columns 3/4/5 — what the pipeline
/// copies in before the call, and what the shop aggregates per car.
pub(super) const WRAP_CAPACITY: usize = 0x08;
pub(super) const WRAP_RUNNING_COST: usize = 0x0c;
pub(super) const WRAP_PRICE: usize = 0x10;
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
pub(super) const ROW_NAME_BASE: usize = 0xa8;
pub(super) const ROW_NAMEALT_BASE: usize = 0xf0;
pub(super) const NAME_LANES: usize = 9;
pub(super) const NAME_LANE_STRIDE: usize = 8;
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
            return Err(format!(
                "spec vector has {count} rows — not the table we think it is"
            ));
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
                 its own train; mod trains must widen the bitmap or fit under it"
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
        let name =
            crate::sys::wide_str(*(scratch.as_ptr().add(ROW_PFORMNAME) as *const *const u16))
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
// or manifest — the boot integration lives in `train.rs`. `harness/host.c` calls this through
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
