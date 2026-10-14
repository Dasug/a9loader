//! The M4 inject mode — window-scoped injection into the family-B loader's
//! worker-row pool (notes/24 round 5, seams design). `pack_serving = "inject"`
//! in a9loader.toml routes `[[pack]]` image effects through this module
//! instead of the M3 cache overlay: no re-encode, no cache file — the mod's
//! decoded payload blocks are appended straight into the pool the game's
//! loader threads consume.
//!
//! # Machine facts this stub is built on (notes/24 R5 + seams.md, 2026-10-19)
//!
//! The family-B loader `pack_loader_image_generic` (FUN_1403efec0) serves
//! me0–3/mtentr0/mtcutp0. At the spliced call site (builds.toml
//! `famb_runner_call`, VA 0x1403f0259) the vanilla sequence has already:
//! 1. decoded the pack into the shared workspace (out base = the value of
//!    `[0x144f86088]`; the decoded stream is `[u32 entry_count]
//!    {0x200 name|w|h|stored|payload}*` where the first u32 IS the count),
//! 2. built a pointer array on ITS OWN STACK (`rsp+0x60`, 8 bytes per
//!    entry, disassembly 0x1403f014f..0x1403f01ac):
//!    `ptr[i] = out_base + running`, `running` starts at **4** and advances
//!    by `stored + 0x20c`,
//! 3. split the entry indices across the worker-row pool
//!    (`chunk_split FUN_1403072b0(&pool, count)` at 0x1403f01b7 — capped at
//!    the worker-row capacity global `DAT_140d65df0`, `count/eff` entries
//!    per row + the remainder spread over the first rows, returns the row
//!    count EBX),
//! 4. refilled every row `< EBX` (two loops, 0x1403f01d5..0x1403f021d and
//!    ..0x1403f0259):
//!    - `row+0x13c` (u32) = this row's FIRST entry index (running start),
//!    - `row+0x140` (u32) = this row's entry count (chunk size),
//!    - `row+0x30`        = the row delegate `FUN_1403f0330` (identical for
//!      every row),
//!    - `row+0x38`        = the SAME stack pointer-array base for every row
//!      (the row's own +0x13c/+0x140 slice into it),
//!    - `row+0x41` (u8)   = the loader's mode byte (same for every row),
//!    then stored the row count at `pool+8` (`mov [0x140f02478], ebx` at
//!    0x1403f021d) — the runner reads the count from there, NOT from EBX.
//!
//! The runner `FUN_1400504c0(&pool)`: SetEvent rows `< count`, then
//! WaitForSingleObject rows `< count` (the 1-row fast path calls the row's
//! delegate inline). Rows beyond `count` are untouched worker slots — the
//! pool is allocated ONCE at boot (FUN_140307040; capacity = the worker
//! count `DAT_140d65df0`; every row gets events/thread then). After the
//! runner the loader scans rows `< EBX` for the failure flag +0x40
//! (delegate-zeroed each run) — EBX (and RBP/RSI/R12B/R13D) are live across
//! our splice, preserved by ABI; the caller's `test ebx,ebx` sits right at
//! site+5. Nothing consumes the runner's RAX (the exit path re-enters at
//! 0x1403f013e and sets AL=1 itself) — we still forward it verbatim.
//!
//! # What the stub does
//!
//! Pack identity comes from the M3 `cfile_open` hook: each planned
//! `data\<pack>.bin` open is recorded (loaders are sequential single-threaded
//! — the capture is coherent when the loader's runner call fires, since the
//! open precedes it inside the loader, seams.md Q1). The stub then extends
//! the LAST vanilla worker row's slice with the mod's entries:
//! - an owned RW `VirtualAlloc` region per pack (built once per window;
//!   pinned for the process lifetime — worker threads read it inside the
//!   window), laid out as
//!   `[ptr array: vanilla_count + ours pointers][our entry blocks]`, where
//!   the vanilla pointers are a verbatim copy of the loader's own array
//!   (every row shares it; row 0's +0x38 points at it) and our blocks are
//!   `{0x200 UTF-16 name NUL + zero pad][u32 w][u32 h][u32 stored][payload]`
//!   (payloads are already the raw decoded bytes — a9-formats parity, M1);
//! - the last row keeps its start (+0x13c), delegate (+0x30) and mode
//!   (+0x41); its count (+0x140) grows by ours and its slice (+0x38) is
//!   re-pointed at our array. The delegate indexes the array ABSOLUTELY
//!   (`array[start + i]`, FUN_1403f0330), so our blocks at indices
//!   `[vanilla_count, vanilla_count+ours)` land exactly in the extended
//!   range. No pool-count bump and no new row: the row pool is saturated
//!   (one row per worker; capacity == worker count — 24/24 live), so a spare
//!   row does not exist.
//! - the runner is then called directly by its own address (base + 0x504C0
//!   for this build — per-build constant alongside the other RvAs below,
//!   the address-book approach for values with no anchor of their own).
//!
//! Vanilla rows other than the last are NOT touched: they keep the loader's
//! stack array for the duration of the runner call (the loader's frame is
//! live across our stub, as vanilla). Any failure, shortfall or unreadable
//! workspace leaves everything untouched and serves the vanilla pool (loud
//! log, once per pack per boot).

use crate::{log, patch, sys};
use a9_mod::manifest::PackEntry;
use core::ffi::c_void;

// ---- per-build constants (international Steam 5.00.5634; values the
// address book carries as behavior, not code bytes) ----

/// FUN_1400504c0 — the worker-row-pool runner, called directly by the stub;
/// the splice's original target is verified against base + this.
const RUNNER_RVA: usize = 0x504C0;
/// `DAT_140d65df0` — worker-row capacity (fixed at boot; read live)
const POOL_CAP_RVA: usize = 0xD65DF0;
/// `DAT_144f86088` — the shared decode workspace's OUT pointer (200 MB)
const WORKSPACE_OUT_RVA: usize = 0x4F86088;

// ---- pinned shape constants (notes/24 + seams.md; memory rule #231) ----

/// worker-row stride
const ROW_STRIDE: usize = 0x148;
/// per-row offsets: delegate, slice-pointer array, first entry index,
/// entry count — the fields the loader's refill writes (the failure flag
/// +0x40 is delegate-managed; we never touch it)
const ROW_DELEGATE: usize = 0x30;
const ROW_ARRAY: usize = 0x38;
const ROW_COUNT: usize = 0x140;
/// entry block header: 0x200 name field + width + height + stored
const ENTRY_HDR: usize = 0x20C;
/// stored-size field inside an entry block (the payload total)
const ENTRY_STORED: usize = 0x208;
/// the entry name field (UTF-16, NUL-terminated, zero pad)
const NAME_FIELD: usize = 0x200;
/// sanity ceiling for one pack's extension region (256 MiB — a runaway mod
/// must refuse, not OOM the game mid-boot)
const MAX_REGION_BYTES: usize = 256 * 1024 * 1024;
/// sanity ceiling for the vanilla entry count read out of the workspace
const MAX_VANILLA_ENTRIES: usize = 1_000_000;

/// One inject op — the engine's plan handover (mirrors the overlay's
/// PendingImageOp without its cache-address fields; payloads stay raw
/// decoded bytes).
pub struct InjectOp {
    pub mod_id: String,
    #[allow(dead_code)] // carried for the plan shape parity with the overlay
    pub mod_version: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub mod_dir: std::path::PathBuf,
    pub file: String,
}

/// The engine's plan: pack name as the manifest spells it → ops in
/// priority order (overlap losers already refused by the resolver).
#[derive(Default)]
pub struct InjectorPlan {
    pub packs: std::collections::BTreeMap<
        String,
        Vec<(std::path::PathBuf, String, String, PackEntry)>,
    >,
}

impl InjectorPlan {
    pub fn is_empty(&self) -> bool {
        self.packs.is_empty()
    }
}

/// Build the stub's ops table from the engine's plan: keys lowercased (the
/// capture path's keys are lowercased opens: `data\mtentr0.bin` →
/// "mtentr0"), values mapped to flat ops. Consumes the plan map; engines
/// clone it first when they need a merge fallback copy.
pub fn plan_ops(
    plan: std::collections::BTreeMap<
        String,
        Vec<(std::path::PathBuf, String, String, PackEntry)>,
    >,
) -> std::collections::BTreeMap<String, Vec<InjectOp>> {
    plan.into_iter()
        .map(|(pack, ops)| {
            (
                pack.to_ascii_lowercase(),
                ops.into_iter()
                    .map(|(dir, id, ver, e)| InjectOp {
                        mod_id: id,
                        mod_version: ver,
                        name: e.name,
                        width: e.width,
                        height: e.height,
                        mod_dir: dir,
                        file: e.file,
                    })
                    .collect(),
            )
        })
        .collect()
}

/// Per-pack runtime state. The region is NEVER freed: the game's worker
/// threads consume it inside the window and its pointers must stay valid
/// for the process lifetime (removal for row-fed content is free — rows
/// are refilled every window — so there is nothing to tear down).
#[allow(dead_code)] // region/ours kept for future diagnostics
struct PackRuntime {
    region: *mut u8,
    ours: usize,
    failing: bool,
}

// Send/Sync justification: the region is written once by the main-thread
// stub BEFORE the runner call (the workers only read it afterwards) and is
// never mutated again; addresses to our own VirtualAlloc allocation.
unsafe impl Send for PackRuntime {}
unsafe impl Sync for PackRuntime {}

struct Inner {
    /// lowercase pack name → ops
    packs: std::collections::BTreeMap<String, Vec<InjectOp>>,
    logger: log::Logger,
    /// "last captured pack": coherence relies on the load sequence being
    /// sequential single-threaded (notes/24 R5: strictly sequential boot
    /// windows) — no compare_exchange, a Mutex only documents the access
    last_pack: std::sync::Mutex<Option<String>>,
    runtime: std::sync::Mutex<std::collections::BTreeMap<String, PackRuntime>>,
    /// base + RUNNER_RVA, resolved once at install (the raw call target)
    runner: usize,
    /// the game image base (all per-build constants are offsets from here)
    base: usize,
}

static G: std::sync::OnceLock<Inner> = std::sync::OnceLock::new();

/// Install the famb splice: verify the site bytes + the original call
/// target against the pinned runner, verify the exe's relay cave (a
/// recorded compiler alignment pad inside the same image), point the site's
/// E8 at the cave and the cave at [`famb_stub`] (absolute), and publish the
/// state before any hook can fire (the engine runs this under the loader
/// lock, before the game's first pack load). `base` is the game image base.
/// `plan` is consumed on success; on Err it stays intact so the engine can
/// fall back to merge.
pub unsafe fn install(
    plan: &mut InjectorPlan,
    base: *const u8,
    site_rva: usize,
    expect: &[u8],
    cave_rva: usize,
    cave_expect: &[u8],
    logger: log::Logger,
) -> Result<(), String> {
    unsafe {
        let site = base.add(site_rva) as *mut u8;
        let cave = base.add(cave_rva) as *mut u8;
        let runner = base.add(RUNNER_RVA) as usize;
        let original = patch::splice_call(
            site,
            expect,
            famb_stub as *const c_void,
            sys::PAGE_EXECUTE_READWRITE,
            Some(runner),
            cave,
            cave_expect,
        )?;
        let packs = plan.packs.keys().cloned().collect::<Vec<_>>();
        let packs_ops = plan_ops(plan.packs.clone());
        let _ = G.set(Inner {
            packs: packs_ops,
            logger,
            last_pack: std::sync::Mutex::new(None),
            runtime: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            runner,
            base: base as usize,
        });
        G.get()
            .unwrap()
            .logger
            .line(&format!(
                "inject: famb splice installed rva={site_rva:#x} expect=verified \
                 (original target {original:#x} = the pool runner; packs: {})",
                packs.join(", ")
            ));
        Ok(())
    }
}

/// The capture seam's receiver (called from the overlay hook's normal
/// path): record `data\<pack>.bin` opens when the injector is live AND the
/// pack carries inject ops. Merge mode never installs G — a no-op there.
/// The capture sticks until the loader's runner call (or a later planned
/// open) consumes/replaces it.
pub fn note_pack_open(pack: &str) {
    if let Some(inner) = G.get()
        && inner.packs.contains_key(pack)
        && let Ok(mut slot) = inner.last_pack.lock()
    {
        *slot = Some(pack.to_string());
    }
}

type RunRaw = unsafe extern "system" fn(pool: *mut u8) -> usize;

/// The spliced famb call site: RCX = &pool (the game sets it right before
/// the E8; the vernier hands it through unchanged). Unknown/failed/no-plan
/// window → the runner crosses the vanilla rows verbatim.
unsafe extern "system" fn famb_stub(pool: *mut u8) -> usize {
    unsafe {
        let Some(inner) = G.get() else {
            // unreachable after install (this fn only exists as the splice
            // target); refuse hard rather than guess
            return 0;
        };
        let pack = inner
            .last_pack
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        let pack = match pack {
            Some(p) => p,
            None => return run_raw(inner.runner, pool),
        };
        if !failing(inner, &pack) {
            if let Some(ops) = inner.packs.get(&pack) {
                match extend_window(inner, row_pool_ok(pool), &pack, ops) {
                    Ok(ours) => inner.logger.line(&format!(
                        "inject famb: {pack} extended by {ours} entr{} \
                         (payloads in loader-owned memory, pinned for the \
                         process lifetime)",
                        if ours == 1 { "y" } else { "ies" }
                    )),
                    Err(e) => {
                        inner.logger.err(&format!(
                            "inject famb: {pack}: {e} — vanilla serving this \
                             boot"
                        ));
                        mark_failing(inner, &pack);
                    }
                }
            }
        }
        run_raw(inner.runner, pool)
    }
}

/// `Ok(pool object base)` when the pool is trustworthy, `Err(reason)` when
/// the stub must fall back untouched. The vanilla refill IS complete at the
/// splice (it runs after the loader's row-fill loops) — we read, rewrite
/// nothing.
unsafe fn row_pool_ok(pool: *mut u8) -> Result<*const u8, String> {
    unsafe {
        if pool.is_null() {
            return Err("null pool pointer".into());
        }
        let object = pool as *const u8;
        if !sys::is_readable(object.cast(), 0x18) {
            return Err("pool object unreadable".into());
        }
        if core::ptr::read(object.add(8) as *const u32) == 0 {
            return Err("no live row refill (count 0)".into());
        }
        Ok(object)
    }
}

/// call the runner raw by address (its RAX comes back through the return
/// slot — the caller ignores it, but the forwarding stays faithful)
unsafe fn run_raw(runner: usize, pool: *mut u8) -> usize {
    unsafe {
        let f: RunRaw = core::mem::transmute(runner);
        f(pool)
    }
}

/// loud-once failed check
fn failing(inner: &Inner, pack: &str) -> bool {
    inner
        .runtime
        .lock()
        .map(|m| m.get(pack).map_or(false, |r| r.failing))
        .unwrap_or(true) // poisoned state: fail closed to vanilla
}

fn mark_failing(inner: &Inner, pack: &str) {
    if let Ok(mut m) = inner.runtime.lock() {
        m.entry(pack.to_string()).or_insert(PackRuntime {
            region: core::ptr::null_mut(),
            ours: 0,
            failing: true,
        });
        if let Some(r) = m.get_mut(pack) {
            r.failing = true;
        }
    }
}

/// The extension: verify the vanilla geometry, read the payloads, build the
/// region, add the appended row. Read-only state until the public pointer
/// writes at the end — the vanilla rows keep the loader's pointers all
/// through the build, so any failure falls back untouched. Returns our
/// appended entry count (for the journal line).
unsafe fn extend_window(
    inner: &Inner,
    pool_object: Result<*const u8, String>,
    pack: &str,
    ops: &[InjectOp],
) -> Result<usize, String> {
    unsafe {
        let pool_object = pool_object?;
        let base = inner.base as *const u8;
        let rows = core::ptr::read(pool_object as *const *mut u8);
        let count = core::ptr::read(pool_object.add(8) as *const u32) as usize;
        let cap = core::ptr::read(base.add(POOL_CAP_RVA) as *const u32) as usize;
        if count == 0 || count > cap {
            return Err(format!(
                "pool row count {count} outside the worker capacity {cap}"
            ));
        }
        if rows.is_null() {
            return Err("pool rows null".into());
        }
        let row0 = rows;
        // the last vanilla row: our entries ride its slice. The pool is
        // saturated (one row per worker; capacity == worker count, and the
        // loader uses all of them for these packs — e.g. 24/24 live), so
        // there is no spare row to append; extending the final slice is the
        // only in-capacity home for injected entries.
        let last = row0.wrapping_add((count - 1) * ROW_STRIDE);
        if !sys::is_readable(last.wrapping_add(ROW_STRIDE - 8).cast(), 8) {
            return Err("row array unreadable".into());
        }
        let delegate = core::ptr::read(row0.add(ROW_DELEGATE) as *const usize);
        if delegate == 0 {
            return Err("vanilla row delegate is null".into());
        }
        // the workspace: vanilla pointers replicate right out of it
        let ws_out = core::ptr::read(base.add(WORKSPACE_OUT_RVA) as *const *mut u8);
        if ws_out.is_null() || !sys::is_readable(ws_out.cast(), ENTRY_HDR) {
            return Err("shared workspace unreadable".into());
        }
        let vanilla_count = core::ptr::read(ws_out as *const u32) as usize;
        if vanilla_count == 0 || vanilla_count > MAX_VANILLA_ENTRIES {
            return Err(format!(
                "sanity: workspace entry count {vanilla_count} out of range"
            ));
        }
        build_and_attach(inner, vanilla_count, row0, count, pack, ops)
    }
}

/// Read every payload, allocate the region, write the vanilla pointer
/// replicate + our entry blocks + the appended row, bump the pool count.
/// The public pointer writes happen only after all files are read.
#[allow(clippy::too_many_arguments)]
unsafe fn build_and_attach(
    inner: &Inner,
    vanilla_count: usize,
    row0: *mut u8,
    row_count: usize,
    pack: &str,
    ops: &[InjectOp],
) -> Result<usize, String> {
    unsafe {
        // 1) payloads first — any unreadable file fails the whole pack
        let payloads = read_payloads(ops)?;
        // 2) sizes + one shared allocation (pointer array + blocks)
        let arr_bytes = (vanilla_count + ops.len())
            .saturating_mul(core::mem::size_of::<usize>());
        let region = alloc_region(arr_bytes, &payloads)?;
        // 3+4) the vanilla pointer replicate and our entry blocks/slots
        let appended = write_region(region, arr_bytes, vanilla_count, row0, ops, &payloads)?;
        // 5) extend the LAST vanilla row + publish the region record
        attach_row(inner, row0, row_count, region, appended, pack);
        Ok(appended)
    }
}

/// Step 1: read every op's payload from its mod dir; any failure names the mod
/// and entry and fails the whole pack.
fn read_payloads(ops: &[InjectOp]) -> Result<Vec<Vec<u8>>, String> {
    let mut payloads = Vec::with_capacity(ops.len());
    for op in ops {
        match std::fs::read(op.mod_dir.join(&op.file)) {
            Ok(b) => payloads.push(b),
            Err(e) => {
                return Err(format!(
                    "mod {} entry {}: payload unreadable: {} ({e})",
                    op.mod_id, op.name, op.file
                ))
            }
        }
    }
    Ok(payloads)
}

/// Step 2: size the one shared region (pointer array + entry blocks) and
/// `VirtualAlloc` it RW. Refuses (never OOMs) when the blocks exceed the
/// sanity ceiling.
unsafe fn alloc_region(arr_bytes: usize, payloads: &[Vec<u8>]) -> Result<*mut u8, String> {
    let mut blocks_bytes = 0usize;
    for p in payloads {
        blocks_bytes = blocks_bytes.saturating_add(ENTRY_HDR + p.len());
    }
    if blocks_bytes > MAX_REGION_BYTES {
        return Err(format!(
            "region size {blocks_bytes} bytes exceeds the \
             {MAX_REGION_BYTES} sanity ceiling"
        ));
    }
    let total = arr_bytes + blocks_bytes;
    let region = unsafe {
        sys::VirtualAlloc(
            core::ptr::null_mut(),
            total,
            sys::MEM_COMMIT | sys::MEM_RESERVE,
            sys::PAGE_READWRITE,
        )
    };
    if region.is_null() {
        return Err("VirtualAlloc for the extension region failed".into());
    }
    Ok(region as *mut u8)
}

/// Steps 3+4: copy the loader's own vanilla pointer array verbatim (every row
/// shares it; row 0's +0x38 points at it) and lay out our entry blocks + their
/// array slots after the vanilla range. Returns the appended entry count.
unsafe fn write_region(
    region: *mut u8,
    arr_bytes: usize,
    vanilla_count: usize,
    row0: *mut u8,
    ops: &[InjectOp],
    payloads: &[Vec<u8>],
) -> Result<usize, String> {
    unsafe {
        // the vanilla pointer replicate — copy the loader's own array
        // verbatim (every row shares it; row 0's +0x38 points at it). Copying
        // avoids depending on the loader's exact `out_base + 4 + Σ` arithmetic.
        let vanilla_array =
            core::ptr::read(row0.add(ROW_ARRAY) as *const *const usize);
        if vanilla_array.is_null()
            || !sys::is_readable(vanilla_array.cast(), vanilla_count * core::mem::size_of::<usize>())
        {
            return Err("vanilla pointer array unreadable".into());
        }
        for j in 0..vanilla_count {
            let p = core::ptr::read(vanilla_array.add(j));
            (region.add(j * core::mem::size_of::<usize>()) as *mut usize).write(p);
        }

        // our entry blocks {0x200 name | w | h | stored | payload} and
        // their array slots after the vanilla range
        let mut w = arr_bytes;
        let mut appended = 0usize;
        for (op, payload) in ops.iter().zip(payloads.iter()) {
            let block = region.add(w);
            write_entry_block(block, op, payload);
            let slot = region.add((vanilla_count + appended) * core::mem::size_of::<usize>())
                as *mut usize;
            slot.write(block as usize);
            w += ENTRY_HDR + payload.len();
            appended += 1;
        }
        Ok(appended)
    }
}

/// Step 5: extend the LAST vanilla row — keep its start (+0x13c) and
/// delegate (+0x30)/mode (+0x41); grow its count (+0x140) and point its slice
/// (+0x38) at our array. The delegate indexes the array ABSOLUTELY
/// (`array[start + i]`, FUN_1403f0330), so our blocks at indices
/// [vanilla_count, vanilla_count+ours) land exactly in this row's extended
/// range. No pool-count bump — the row already exists. Then publish the region
/// record (builds are one-shot per pack per boot; a second window re-reads and
/// re-appends harmlessly).
unsafe fn attach_row(
    inner: &Inner,
    row0: *mut u8,
    row_count: usize,
    region: *mut u8,
    appended: usize,
    pack: &str,
) {
    unsafe {
        let last = row0.wrapping_add((row_count - 1) * ROW_STRIDE);
        let last_count =
            core::ptr::read(last.add(ROW_COUNT) as *const u32) as usize;
        core::ptr::write(last.add(ROW_ARRAY) as *mut usize, region as usize);
        core::ptr::write(
            last.add(ROW_COUNT) as *mut u32,
            (last_count + appended) as u32,
        );

        if let Ok(mut m) = inner.runtime.lock() {
            m.entry(pack.to_string())
                .or_insert(PackRuntime {
                    region: core::ptr::null_mut(),
                    ours: 0,
                    failing: false,
                })
                .region = region;
        }
    }
}

/// Write one entry block: `{0x200 name UTF-16 NUL-terminated zero-pad | u32
/// width | u32 height | u32 stored | payload}`. Our blocks encode the name
/// field strictly (NUL + zero pad — the original packs keep garbage in the
/// pad, consumers read to the NUL, both work; NameField semantics per the
/// M1 findings in memory rule #231).
fn write_entry_block(block: *mut u8, op: &InjectOp, payload: &[u8]) {
    let head: String = op.name.chars().take(NAME_FIELD / 2 - 1).collect();
    let name_units = head.encode_utf16().collect::<Vec<u16>>();
    unsafe {
        core::ptr::write_bytes(block, 0, NAME_FIELD);
        for (i, u) in name_units.iter().enumerate() {
            core::ptr::write(block.add(i * 2) as *mut u16, *u);
        }
        core::ptr::write(block.add(0x200) as *mut u32, op.width);
        core::ptr::write(block.add(0x204) as *mut u32, op.height);
        core::ptr::write(block.add(ENTRY_STORED) as *mut u32, payload.len() as u32);
        core::ptr::copy_nonoverlapping(
            payload.as_ptr(),
            block.add(ENTRY_HDR),
            payload.len(),
        );
    }
}
