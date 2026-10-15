//! the **save-id clamp guard** (always on; a behaviour fix, NOT a
//! diagnostic, and deliberately not behind a config flag).
//!
//! A saved formation's form id is used as a spec-row index with **no bounds
//! check**, so an id past the vector's length
//! (`[0x140e14528]`, stride `0x140`) reads past the heap block — a garbage
//! train, or a crash for larger ids. That is reachable today whenever a save is
//! loaded without the mod that made it, and becomes routine once mod-added
//! trains exist past row 421. This guard makes any out-of-range id degrade to the
//! engine's own invalid-row state instead.
//!
//! # The two choke points
//!
//! Every saved id that reaches the spec table flows through one of these, so a
//! single clamp each covers all the consumers §5 lists:
//!
//! 1. **the group head** — the u32 id in `save_train_group_restore`
//!    (0x1401b6d10), clamped right after the legacy remap and immediately
//!    before the row test (`0x1401b6f0c`). Covers the function's own blank/
//!    normal branch and `roster_row_builder` (0x140060f50), which is only ever
//!    called with this (now in-range) id.
//! 2. **the per-car ids** — the fixed `0xf8` car block reader bulk-
//!    reads into the consist object (`call mmf_paged_read` at 0x140061f8b,
//!    destination `consist+0x50c`). Every per-car u16 id (10 rows, stride
//!    `0x18`) comes from that block, so clamping it right after the read covers
//!    `train_row_car_wiring` (0x1401ab990), `custom_dat_parser` (0x140061260)
//!    and `train_form_readout_update` (0x140061650) at once — the latter two are
//!    called from *inside* that reader after the read, and the first reads
//!    the same (now-clamped) block later.
//!
//! # Semantics (fail open into the engine's own degraded state)
//!
//! An out-of-range id is rewritten to a shipped placeholder row, and everything
//! downstream then behaves exactly as it does for that row:
//!
//! * head → **417**, the engine's own auto-named invalid row (`form_id_417`,
//!   written by `train_list_builder`'s tail 0x14019d2dd): `bValid == 0`,
//!   `bSpecialFlag == 0`, valid `pFormName` ⇒ the blank/placeholder branch at
//!   0x1401b6f36 runs and clones that name. The row is never a CSV row and
//!   never a mod slot, so it can never be repurposed — the blank degrade is
//!   permanent. (A zero-filled row would crash there: the branch dereferences
//!   `pFormName`, §1.4.)
//! * per-car → **160** = `FORM_DUMMY_03` (有効=OFF ⇒ `bValid == 0`; a fully
//!   initialised row whose model is the shipped `dummy/_t_dummy_a_.x`
//!   placeholder). A car row MUST be fully initialised: rows 417–420 have raw
//!   `operator new` memory for `aModelIdx` (`spec_table_realloc` 0x14019fc50
//!   never zeroes), so a car clamped there would index the model table with
//!   garbage. 160 is not the slot the shipped `new-train-template` repurposes
//!   (`FORM_DUMMY_02` = 62).
//!
//! Both targets are read from the LIVE vector length
//! (`([0x140e14530] - [0x140e14528]) / 0x140`), so the guard stays correct after
//! the injection grows the vector. It is **provably inert for every in-range id** (the
//! comparison is strict `<`), and every failure path leaves the id untouched
//! and logs loudly at most once.
//!
//! # Mechanics
//!
//! Same-image relay caves (`FF 25` + imm64) and the `patch.rs` helpers, exactly
//! like the inject splices:
//!
//! * the head site's 7 bytes are rewritten to `E8 rel32; 90 90` (a `call` to
//!   the cave); a naked shim in version.dll clamps `r14d`, reproduces the two
//!   overwritten instructions (`movsxd rax,r14d; lea rcx,[rax+rax*4]`) and
//!   `ret`s to `site+5` (the 2 nops) → the original `shl rcx,0x6`. `RSP ≡ 0`
//!   (mod 16) at the site, so the shim presents the ABI's `RSP ≡ 8` to the Rust
//!   clamp fn.
//! * the car site's `E8` is spliced with [`patch::splice_to_cave`] to a stub
//!   that calls the original reader with the caller's exact arguments, clamps
//!   the returned block, and returns the reader's own result.
//!
//! Every anchor/expect mismatch refuses **before any write** and leaves the
//! game vanilla (fail-open, logged loudly).
//!
//! # Logging
//!
//! * `log_level = info`: **one** line per SITE, emitted on the first clamp at
//!   that site — the site, the original id, the target, the vector size and the
//!   fact that re-saving now persists the placeholder row. Per-site (not
//!   per-boot) on purpose: the head clamp and the car clamp are independent
//!   choke points, and a boot that clamps cars first must not swallow the head
//!   site's line (the earlier single `summary_logged` flag did exactly that —
//!   the head path's diagnostics were invisible in a live save that hit the car
//!   clamp first).
//! * `log_level = debug`: one line per distinct (site, original id), plus a
//!   per-site count milestone at 10/100/1000/… clamps, so a save full of bad
//!   ids cannot flood the log but the total is still recoverable.

use crate::{log, patch, sys};
use a9_mod::builds::BuildEntry;
use core::ffi::c_void;
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

/// The group-head clamp site (builds.toml).
pub const ANCHOR_HEAD: &str = "save_group_head";
/// The group-head relay cave (builds.toml).
pub const ANCHOR_HEAD_CAVE: &str = "save_group_head_cave";
/// The per-car read call site (builds.toml).
pub const ANCHOR_CAR: &str = "save_car_read";
/// The per-car relay cave (builds.toml).
pub const ANCHOR_CAR_CAVE: &str = "save_car_read_cave";

/// The rewritten head region: `movsxd rax,r14d; lea rcx,[rax+rax*4]` (7 bytes),
/// replaced by `E8 rel32; 90 90`.
const HEAD_LEN: usize = 7;
/// The spec-table manager singleton (0x140e14510).
const MANAGER_RVA: usize = 0xE14510;
/// The manager's rows begin/end fields (a `std::vector<row>` at manager+0x18).
const MGR_ROWS: usize = 0x18;
const MGR_ROWS_END: usize = 0x20;
/// The spec-row stride.
const ROW_STRIDE: usize = 0x140;
/// The fixed car section: 10 rows of 0x18 (the trailing 8 bytes are the
/// count/trailer and are never touched).
const CAR_ROWS: usize = 10;
const CAR_STRIDE: usize = 0x18;
/// The exact size of the car block the site reads.
const CAR_BLOCK: u32 = 0xf8;

/// The head clamp target: the engine's own auto-named invalid row 417.
const HEAD_TARGET: u32 = 417;
/// The per-car clamp target: FORM_DUMMY_03 (id 160) — a fully-initialised
/// invalid placeholder row.
const CAR_TARGET: u16 = 160;

/// `mmf_paged_read(ring, dest, size)` — the byte-level paged reader the car
/// block flows through.
type ReadFn = unsafe extern "system" fn(*mut c_void, *mut u8, u32) -> u64;

/// Immutable install-time state.
struct Config {
    base: usize,
    /// the original `mmf_paged_read` the car stub forwards to
    read: usize,
}

static CFG: OnceLock<Config> = OnceLock::new();

/// Mutable logging/dedup state (save loading is single-threaded, but the guard
/// is callable from the game's own threads — lock it).
struct LogState {
    logger: log::Logger,
    debug: bool,
    /// one info line per site (0 = head, 1 = car), emitted on that site's
    /// first clamp — per-site, not per-boot (see the module doc)
    summary_logged: [bool; 2],
    /// clamps seen per site (0 = head, 1 = car)
    counts: [u64; 2],
    /// next count milestone to report per site
    next_milestone: [u64; 2],
    /// (site, original id) -> occurrences (distinct-id dedup)
    seen: BTreeMap<(u8, u32), u64>,
}

static LOG: OnceLock<Mutex<LogState>> = OnceLock::new();

const SITE_HEAD: u8 = 0;
const SITE_CAR: u8 = 1;

fn site_name(site: u8) -> &'static str {
    if site == SITE_HEAD {
        ANCHOR_HEAD
    } else {
        ANCHOR_CAR
    }
}

/// The live spec-vector row count, or `None` when the manager is unreadable /
/// nonsensical (fail-open: the caller leaves the id untouched).
unsafe fn row_count(base: usize) -> Option<usize> {
    unsafe {
        let m = (base + MANAGER_RVA) as *const u8;
        if !sys::is_readable(m.add(MGR_ROWS).cast(), 16) {
            return None;
        }
        let begin = *(m.add(MGR_ROWS) as *const usize);
        let end = *(m.add(MGR_ROWS_END) as *const usize);
        let span = end.checked_sub(begin)?;
        let rows = span / ROW_STRIDE;
        // a sane cap: the reserved range stays ≤ 65535 (the per-car id is u16),
        // so anything past that means the manager is not what we think it is.
        if rows == 0 || rows > 0x10000 {
            return None;
        }
        Some(rows)
    }
}

/// Record one clamp and emit whatever it warrants. `rows` is the vector size at
/// the moment of the clamp (for the log line).
/// One debug line per boot for the slot repair (it is not an id clamp, and it is
/// the normal state for a save written before the row carried a car block).
fn note_slot(site: u8, car: usize) {
    static ONCE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    if ONCE.swap(true, core::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let Some(m) = LOG.get() else { return };
    let Ok(st) = m.lock() else { return };
    if st.debug {
        st.logger.debug(&format!(
            "save clamp [{}]: car block row {car} carried slot 0 — repaired to {} \
             (the game's car wiring computes slot - 1 and would walk a wild index)",
            site_name(site),
            1 + car
        ));
    }
}

fn note_clamp(site: u8, orig: u32, target: u32, rows: usize) {
    let Some(m) = LOG.get() else { return };
    let Ok(mut st) = m.lock() else { return };
    let s = site as usize;
    st.counts[s] += 1;
    let key = (site, orig);
    let first = !st.seen.contains_key(&key);
    *st.seen.entry(key).or_insert(0) += 1;
    let name = site_name(site);
    if first && st.debug {
        st.logger.debug(&format!(
            "save clamp [{name}]: out-of-range save id {orig} -> {target} \
             (spec vector has {rows} rows)"
        ));
    }
    // the "final count" (log-scale, flood-proof): a milestone line at
    // 10/100/1000/… clamps per site, at debug level.
    if st.counts[s] >= st.next_milestone[s] {
        if st.debug {
            st.logger.debug(&format!(
                "save clamp [{name}]: {} out-of-range id(s) clamped so far",
                st.counts[s]
            ));
        }
        st.next_milestone[s] = st.next_milestone[s].saturating_mul(10);
    }
    if !st.summary_logged[s] {
        st.summary_logged[s] = true;
        st.logger.line(&format!(
            "save clamp [{name}]: out-of-range save id {orig} clamped to {target} \
             (spec vector has {rows} rows); re-saving now persists the placeholder \
             row — every distinct id is logged at log_level=debug"
        ));
    }
}

/// The head clamp: `extern "system"` so the naked shim can `call` it. Returns
/// the id unchanged when in range (or when the manager is unreadable).
unsafe extern "system" fn head_clamp(id: u32) -> u32 {
    let Some(cfg) = CFG.get() else { return id };
    let Some(rows) = (unsafe { row_count(cfg.base) }) else {
        return id;
    };
    if (id as usize) < rows {
        return id;
    }
    note_clamp(SITE_HEAD, id, HEAD_TARGET, rows);
    HEAD_TARGET
}

/// Clamp the 10 per-car u16 ids in the freshly-read block at `dest` and repair a
/// zero car slot. Inert for every in-range id; a row whose id is 0 (unused)
/// stays untouched.
///
/// The block's per-car entry is `[u16 form id][u8 slot][u8 flags]` + padding —
/// the same layout the consist object carries at `+0x50c`, `+0x50e`. The slot is
/// the car's 1-based position, and `train_row_car_wiring` (0x1401ab990) uses it
/// as `aModelIdx[slot - 1]`: a **zero** slot therefore computes index `-1` and
/// walks a wild index into the spec table — live crash 2026-10-15
/// (`0x1401aba4e`, `slot - 1` = 0xFFFFFFFF, faulting read past the heap). A save
/// written while the row's per-car block was still empty carries that zero, so
/// repair it here, where the same block is already in hand.
unsafe fn clamp_car_block(cfg: &Config, dest: *mut u8) {
    let Some(rows) = (unsafe { row_count(cfg.base) }) else {
        return;
    };
    for i in 0..CAR_ROWS {
        let entry = unsafe { dest.add(i * CAR_STRIDE) };
        let slot = entry as *mut u16;
        let id = unsafe { core::ptr::read_unaligned(slot) };
        if id == 0 {
            continue;
        }
        // the car's slot: 1-based, so 0 is never valid
        let slot_byte = unsafe { entry.add(2) };
        if unsafe { *slot_byte } == 0 {
            unsafe { *slot_byte = (1 + i) as u8 };
            note_slot(SITE_CAR, i);
        }
        if (id as usize) < rows {
            continue;
        }
        note_clamp(SITE_CAR, id as u32, CAR_TARGET as u32, rows);
        unsafe { core::ptr::write_unaligned(slot, CAR_TARGET) };
    }
}

/// The spliced car reader: forward the caller's arguments to the original
/// `mmf_paged_read`, clamp the returned block when it is the fixed `0xf8` car
/// section, and return the original's own result.
unsafe extern "system" fn car_stub(ring: *mut c_void, dest: *mut u8, size: u32) -> u64 {
    let Some(cfg) = CFG.get() else { return 0 };
    let read: ReadFn = unsafe { core::mem::transmute(cfg.read) };
    let r = unsafe { read(ring, dest, size) };
    if size == CAR_BLOCK
        && !dest.is_null()
        && unsafe { sys::is_readable(dest.cast(), CAR_BLOCK as usize) }
    {
        unsafe { clamp_car_block(cfg, dest) };
    }
    r
}

// The naked head shim: the relay cave jumps here (the site's `E8` pushed
// `site+5`). At entry `RSP ≡ 8 (mod 16)`; `sub rsp,0x28` gives the Rust fn its
// shadow space and the ABI alignment, then the shim reproduces the two
// instructions the rewrite overwrote and `ret`s to `site+5` (2 nops → the
// original `shl rcx,0x6`). r14 is callee-saved, so the Rust fn preserves it.
core::arch::global_asm!(
    ".globl a9_clamp_head_shim",
    ".p2align 4",
    "a9_clamp_head_shim:",
    "sub rsp, 0x28",
    "mov ecx, r14d",
    "call {clamp}",
    "mov r14d, eax",
    "add rsp, 0x28",
    "movsxd rax, r14d",
    "lea rcx, [rax+rax*4]",
    "ret",
    clamp = sym head_clamp,
);

unsafe extern "C" {
    fn a9_clamp_head_shim();
}

fn shim_addr() -> *const c_void {
    let f: unsafe extern "C" fn() = a9_clamp_head_shim;
    f as *const c_void
}

/// Install both clamps. Every anchor is expect-verified before any write; a
/// mismatch refuses loudly and leaves the game vanilla (fail-open). Returns the
/// journal line.
pub unsafe fn install(
    entry: &BuildEntry,
    base: *const u8,
    logger: log::Logger,
    debug: bool,
) -> Result<String, String> {
    unsafe {
        let head = entry
            .anchor(ANCHOR_HEAD)
            .ok_or_else(|| format!("this build has no {ANCHOR_HEAD} anchor"))?;
        let head_cave = entry
            .anchor(ANCHOR_HEAD_CAVE)
            .ok_or_else(|| format!("this build has no {ANCHOR_HEAD_CAVE} anchor"))?;
        let car = entry
            .anchor(ANCHOR_CAR)
            .ok_or_else(|| format!("this build has no {ANCHOR_CAR} anchor"))?;
        let car_cave = entry
            .anchor(ANCHOR_CAR_CAVE)
            .ok_or_else(|| format!("this build has no {ANCHOR_CAR_CAVE} anchor"))?;

        let head_expect = head.expect_bytes().unwrap_or_default();
        let car_expect = car.expect_bytes().unwrap_or_default();
        if head_expect.len() < HEAD_LEN {
            return Err(format!(
                "{ANCHOR_HEAD} records {} expect bytes, the rewrite needs {HEAD_LEN}",
                head_expect.len()
            ));
        }
        if car_expect.len() < 5 {
            return Err(format!(
                "{ANCHOR_CAR} records {} expect bytes, the E8 splice needs 5",
                car_expect.len()
            ));
        }
        let head_site = base as usize + head.rva as usize;
        let head_cave_addr = base as usize + head_cave.rva as usize;
        let car_site = base as usize + car.rva as usize;
        let car_cave_addr = base as usize + car_cave.rva as usize;

        // verify the head region (7 bytes) before touching anything
        if !sys::is_readable(head_site as *const c_void, HEAD_LEN) {
            return Err(format!("{ANCHOR_HEAD} rva {:#x} is not readable memory", head.rva));
        }
        if std::slice::from_raw_parts(head_site as *const u8, HEAD_LEN) != &head_expect[..HEAD_LEN] {
            return Err(format!(
                "{ANCHOR_HEAD} bytes differ from the address book (game update?) — refusing"
            ));
        }
        // the car site's original reader, derived from the book's own E8 rel32 —
        // the stub forwards to it (and the splice pins the same value).
        let read_target = {
            let rel = i32::from_le_bytes(car_expect[1..5].try_into().unwrap());
            (car_site + 5).wrapping_add(rel as isize as usize)
        };

        // publish the runtime config BEFORE any splice: the stubs are only ever
        // reachable through a cave that a successful install wrote, so a partial
        // failure (a refused anchor) leaves them unreachable.
        let _ = CFG.set(Config {
            base: base as usize,
            read: read_target,
        });
        let _ = LOG.set(Mutex::new(LogState {
            logger,
            debug,
            summary_logged: [false, false],
            counts: [0, 0],
            next_milestone: [10, 10],
            seen: BTreeMap::new(),
        }));

        // the head relay cave: FF 25 + imm64 -> the naked shim
        patch::install_relay_cave(
            head_cave_addr as *mut u8,
            &head_cave.expect_bytes().unwrap_or_default(),
            shim_addr(),
            sys::PAGE_EXECUTE_READWRITE,
        )?;
        // rewrite the 7-byte region: `call cave; nop; nop`
        let mut b = [0x90u8; HEAD_LEN];
        b[0] = 0xE8;
        let rel = head_cave_addr as i64 - (head_site as i64 + 5);
        if !(i32::MIN as i64..=i32::MAX as i64).contains(&rel) {
            return Err(format!(
                "cave {head_cave_addr:#x} out of rel32 range of the head site {head_site:#x} — \
                 the book records them from the same image, this cannot happen"
            ));
        }
        b[1..5].copy_from_slice(&(rel as i32).to_le_bytes());
        if !patch::write_bytes(head_site as *mut u8, &b, sys::PAGE_EXECUTE_READWRITE) {
            return Err("writing the rewritten head site failed".into());
        }

        // the per-car splice: the site's E8 -> the relay cave -> car_stub
        let original = patch::splice_call(
            car_site as *mut u8,
            &car_expect,
            car_stub as *const c_void,
            sys::PAGE_EXECUTE_READWRITE,
            Some(read_target),
            car_cave_addr as *mut u8,
            &car_cave.expect_bytes().unwrap_or_default(),
        )?;

        Ok(format!(
            "save clamp: head rva={:#x} expect=verified (out-of-range head id -> row {HEAD_TARGET}); \
             car rva={:#x} expect=verified (original reader {original:#x}, out-of-range car id -> \
             row {CAR_TARGET})",
            head.rva, car.rva
        ))
    }
}
