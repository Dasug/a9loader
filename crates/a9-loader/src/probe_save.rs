//! DIAGNOSTIC ONLY — OFF BY DEFAULT, not part of normal loader operation.
//!
//! Nothing in this module runs unless `probe_save = true` is set in
//! `a9loader.toml`; a default boot never installs these hooks. It exists to
//! answer two one-off reverse-engineering questions about the save path
//!. It serves nothing, changes no behaviour, and every hook
//! is trampoline-only, forwarding verbatim to the original.
//!
//! The questions:
//!  1. Is a formation's car block written as ONE bulk `0xf8` write (⇒ the
//!     20-byte per-car tails round-trip verbatim, so the per-car tail
//!     provenance works) or as ten `0x18` writes / a per-car write (⇒ the
//!     tails are regenerated and Tier 1 is out)?
//!  2. Which caller RVA is the group-record writer? `save_mmf_write_block` has
//!     only three static callers; a fourth at save time is the writer, and its
//!     RVA is the site the boot will patch to substitute the placeholder index.
//!
//! WHAT IT HOOKS — the four save-side mmf helpers (the byte-level writers
//! every save field flows through, plus the u32-vector block pair the group
//! record's vector fields use):
//!  - `mmf_paged_write`      `0x140308fe0` — the byte-level save writer;
//!  - `mmf_paged_write_bulk` `0x140308e20` — the bulk save writer;
//!  - `save_mmf_write_block` `0x140041500` — the u32-vector block writer, i.e.
//!    the target of the vtable thunk `0x140115f70` (`LEA RDX,[RCX+0x10];
//!    JMP save_mmf_write_block`); hooking the callee catches ALL its callers
//!    with their return addresses — a fourth one at save time is the writer;
//!  - `mmf_read_u32_vector`  `0x140041820` — **READ-SIDE**, the target of the
//!    vtable thunk `0x140115f80`; labelled in every line it produces.
//!
//! WHY NOT HOOK THE TWO THUNKS THEMSELVES (`0x140115f70` / `0x140115f80`):
//! each is a 9-byte body (`LEA RDX,[RCX+0x10]` + `JMP rel32`) followed by INT3
//! padding, and [`patch::install_trampoline`] refuses an `expect` shorter than
//! its 12-byte hook write and re-enters the original at `entry + expect.len()`
//! — for a 9-byte thunk that re-entry lands in the padding (INT3) or in the
//! adjacent thunk. Hooking the callees gives the same coverage plus the
//! byte-level writes that actually answer question 1. Note that because both
//! thunks are `JMP`s (no `call`), the return address seen at the callee is the
//! *thunk's* caller, not the thunk itself — which is the more useful datum.
//!
//! ABI (each machine-verified against the build's own code, not assumed):
//!  - `mmf_paged_write(rcx = ring, rdx = src buffer, r8 = size)` — void;
//!  - `mmf_paged_write_bulk(rcx = ring, rdx = src buffer, r8 = size)` — void;
//!  - `save_mmf_write_block(rcx = unused, rdx = u32-vector* {begin,end})` —
//!    void; writes `[u32 count][count * u32]` (count = (end-begin)/4);
//!  - `mmf_read_u32_vector(rcx = unused, rdx = u32-vector*)` — void; reads a
//!    u32 count then count*4 bytes into the vector (READ-side).
//!
//! The RETURN ADDRESS: the game `call`s the entry, which the installed
//! `jmp [rip+0]; imm64` turns into a jump — so at the hook's first instruction
//! `[rsp]` IS the caller's continuation. A naked `global_asm!` shim reads it
//! into a free volatile argument register (`r9` for the 3-arg writers, `r8`
//! for the 2-arg vector helpers) and tail-jumps to the Rust body, which
//! forwards every original argument verbatim through its trampoline. Stack
//! discipline: at a hooked entry `RSP ≡ 8 (mod 16)`; the shim's `jmp` preserves
//! that, which is exactly the alignment a called function expects, so the
//! body's `ret` returns to the game with the stack untouched.
//!
//! Log volume: `mmf_paged_write` is the save file's ONE writer, so a save
//! calls it many times. Per-call detail is therefore logged only for a WATCH
//! set of sizes (the car-block shapes `0x8`/`0x18`/`0xf0`/`0xf8`, the roster
//! head `0x2a`, and the two verbatim state blocks `0x124`/`0xbc0`), capped per
//! size, plus one line per DISTINCT caller RVA (the save-side write-site map)
//! and a periodic size histogram. Every failure path logs one loud line and
//! leaves the boot vanilla (fail-open, like the other probes).

use crate::{log, patch, sys};
use a9_mod::builds::BuildEntry;
use core::ffi::c_void;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::OnceLock;

/// The byte-level save writer (builds.toml anchor).
pub const ANCHOR_PW: &str = "mmf_paged_write";
/// The bulk save writer (builds.toml anchor).
pub const ANCHOR_PWB: &str = "mmf_paged_write_bulk";
/// The u32-vector block writer — the `0x140115f70` thunk's JMP target.
pub const ANCHOR_WBLOCK: &str = "save_mmf_write_block";
/// The u32-vector block READER — the `0x140115f80` thunk's JMP target.
pub const ANCHOR_RVEC: &str = "mmf_read_u32_vector";

/// Sizes whose per-call detail is logged (the shapes the two questions turn
/// on). Each size is capped at [`DETAIL_PER_SIZE`] so a common size cannot
/// starve a rare one.
const WATCH_SIZES: [u32; 7] = [0x8, 0x18, 0x2a, 0xf0, 0xf8, 0x124, 0xbc0];
/// Per-size cap on the detailed per-call lines.
const DETAIL_PER_SIZE: usize = 40;
/// The read helper carries no size argument (it reads a u32 count from the
/// stream); its calls are logged separately, capped here.
const READ_DETAIL_CAP: usize = 60;
/// One `save progress` histogram line every this many calls.
const PROGRESS_EVERY: u64 = 4096;
/// Bytes of the written buffer logged per call.
const MAX_DATA: usize = 16;

struct Inner {
    logger: log::Logger,
    /// the game image base (caller RVAs are `ret - base`)
    base: usize,
    /// trampolines in install order: pw, pwb, wblock, rvec
    tramp: [Option<patch::Trampoline>; 4],
    /// caller RVA -> call count (the save-side write-site map)
    callers: BTreeMap<usize, u64>,
    /// size -> call count (the histogram)
    sizes: BTreeMap<u32, u64>,
    /// size -> detailed lines already emitted
    detail: BTreeMap<u32, usize>,
    /// detailed read-side lines already emitted
    read_detail: usize,
    calls: u64,
    next_progress: u64,
}

static G: OnceLock<Mutex<Inner>> = OnceLock::new();

/// Install the diagnostic hooks. Each anchor is expect-verified; a failure of
/// one is reported in the returned lines and the others still install.
pub unsafe fn install(
    logger: log::Logger,
    base: *const u8,
    entry: &BuildEntry,
) -> Result<Vec<String>, String> {
    unsafe {
        let _ = G.set(Mutex::new(Inner {
            logger,
            base: base as usize,
            tramp: [None, None, None, None],
            callers: BTreeMap::new(),
            sizes: BTreeMap::new(),
            detail: BTreeMap::new(),
            read_detail: 0,
            calls: 0,
            next_progress: PROGRESS_EVERY,
        }));
        // (anchor name, hook shim, log label, trampoline slot)
        let specs: [(&str, *const c_void, &str, usize); 4] = [
            (
                ANCHOR_PW,
                shim(a9_save_pw_shim),
                "mmf_paged_write (the byte-level save writer)",
                0,
            ),
            (
                ANCHOR_PWB,
                shim(a9_save_pwb_shim),
                "mmf_paged_write_bulk (the bulk save writer)",
                1,
            ),
            (
                ANCHOR_WBLOCK,
                shim(a9_save_wblock_shim),
                "save_mmf_write_block (the u32-vector block writer — the 0x140115f70 thunk's target)",
                2,
            ),
            (
                ANCHOR_RVEC,
                shim(a9_save_rvec_shim),
                "mmf_read_u32_vector (READ-side — the 0x140115f80 thunk's target)",
                3,
            ),
        ];
        let mut lines = Vec::new();
        let mut installed = 0usize;
        for (name, hook, what, slot) in specs {
            match entry.anchor(name) {
                Some(a) => match patch::install_trampoline(
                    base.cast_mut().add(a.rva as usize),
                    hook,
                    &a.expect_bytes().unwrap_or_default(),
                    sys::PAGE_EXECUTE_READWRITE,
                ) {
                    Ok(t) => {
                        if let Ok(mut g) = G.get().unwrap().lock() {
                            g.tramp[slot] = Some(t);
                        }
                        lines.push(format!(
                            "probe: {what} hook installed rva={:#x} expect=verified",
                            a.rva
                        ));
                        installed += 1;
                    }
                    Err(e) => lines.push(format!(
                        "probe refused: save {what} rva={:#x}: {e}",
                        a.rva
                    )),
                },
                None => lines.push(format!(
                    "probe refused: this build has no {name} anchor (older address book?)"
                )),
            }
        }
        if installed == 0 {
            return Err("all save probe anchors refused".into());
        }
        Ok(lines)
    }
}

// The naked entry shims. The installed hook jumps here (the game's `call`
// pushed its continuation); each reads that return address into a free volatile
// argument register and tail-jumps to the Rust body, leaving the stack exactly
// as the original call left it.
//
// 3-arg writers: rcx/rdx/r8 are the arguments, so r9 carries the return
// address. 2-arg vector helpers: rcx/rdx are the arguments, so r8 carries it.
// At entry `RSP ≡ 8 (mod 16)`; the `jmp` preserves that for the body.
core::arch::global_asm!(
    ".globl a9_save_pw_shim",
    ".p2align 4",
    "a9_save_pw_shim:",
    "mov r9, [rsp]",
    "jmp {body}",
    body = sym pw_body,
);
core::arch::global_asm!(
    ".globl a9_save_pwb_shim",
    ".p2align 4",
    "a9_save_pwb_shim:",
    "mov r9, [rsp]",
    "jmp {body}",
    body = sym pwb_body,
);
core::arch::global_asm!(
    ".globl a9_save_wblock_shim",
    ".p2align 4",
    "a9_save_wblock_shim:",
    "mov r8, [rsp]",
    "jmp {body}",
    body = sym wblock_body,
);
core::arch::global_asm!(
    ".globl a9_save_rvec_shim",
    ".p2align 4",
    "a9_save_rvec_shim:",
    "mov r8, [rsp]",
    "jmp {body}",
    body = sym rvec_body,
);

unsafe extern "C" {
    fn a9_save_pw_shim();
    fn a9_save_pwb_shim();
    fn a9_save_wblock_shim();
    fn a9_save_rvec_shim();
}

fn shim(f: unsafe extern "C" fn()) -> *const c_void {
    f as *const c_void
}

/// A human label for the caller RVAs the address book already knows, so the
/// log reads like the open-path list in `a9ml report`. Unknown sites print nothing (and are
/// exactly what question 2 is looking for).
fn caller_label(rva: usize) -> &'static str {
    match rva {
        0x115F69 => " [the 0x140115f64 direct call — the 3-caller set]",
        0x110FF4 => " [the 0x140110fef call — the 3rd save_mmf_write_block caller]",
        0x1B8008 => " [save_train_group_restore — the group reader, call 1]",
        0x1B8014 => " [save_train_group_restore — the group reader, call 2]",
        _ => "",
    }
}

/// The first [`MAX_DATA`] bytes at `buf` as hex, or `<unreadable>`.
unsafe fn peek(buf: *const u8) -> String {
    unsafe {
        if buf.is_null() || !sys::is_readable(buf.cast(), MAX_DATA) {
            return "<unreadable>".into();
        }
        let b = std::slice::from_raw_parts(buf, MAX_DATA);
        b.iter()
            .map(|x| format!("{x:02x}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The top sizes in the histogram (at most 8), `0x<size>×<count>`.
fn top_sizes(sizes: &BTreeMap<u32, u64>) -> String {
    let mut v: Vec<(u32, u64)> = sizes.iter().map(|(k, c)| (*k, *c)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v.truncate(8);
    v.iter()
        .map(|(s, c)| format!("0x{s:x}×{c}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Record one helper call and emit whatever it warrants: the caller-site map
/// line (once per distinct RVA), the two verbatim-block lines, the capped
/// per-size detail, and the periodic histogram.
unsafe fn note(g: &mut Inner, name: &'static str, ret: usize, size: Option<u32>, buf: *const u8) {
    unsafe {
        g.calls += 1;
        let rva = ret.wrapping_sub(g.base);
        let sz = size.unwrap_or(0);
        // the save-side write-site map: one line per DISTINCT caller RVA
        let first = !g.callers.contains_key(&rva);
        *g.callers.entry(rva).or_insert(0) += 1;
        if let Some(s) = size {
            *g.sizes.entry(s).or_insert(0) += 1;
        }
        if first {
            let what = if size.is_some() { "write" } else { "read" };
            let szs = match size {
                Some(s) => format!("0x{s:x}"),
                None => "-".into(),
            };
            g.logger.line(&format!(
                "probe: save caller#{n} {what} site=0x{rva:x} size={szs} helper={name}{}",
                caller_label(rva),
                n = g.callers.len(),
            ));
        }
        // the two verbatim state blocks (the second storage candidate)
        if sz == 0x124 || sz == 0xbc0 {
            let which = if sz == 0x124 {
                "state block 0x124 — the 292-byte verbatim region"
            } else {
                "mini-block 0xbc0 — the 3008-byte verbatim region"
            };
            g.logger.line(&format!(
                "probe: save verbatim block size=0x{sz:x} caller=0x{rva:x} ({which}){}",
                caller_label(rva)
            ));
        }
        // capped per-size detail for the watched shapes
        if let Some(s) = size
            && WATCH_SIZES.contains(&s)
        {
            let n = g.detail.entry(s).or_insert(0);
            if *n < DETAIL_PER_SIZE {
                *n += 1;
                let data = peek(buf);
                g.logger.line(&format!(
                    "probe: save write {name} ret=0x{ret:x} caller=0x{rva:x} size=0x{s:x} \
                     data={data}{}",
                    caller_label(rva)
                ));
            }
        }
        // the read helper carries no size: log its calls separately, capped
        if size.is_none() && g.read_detail < READ_DETAIL_CAP {
            g.read_detail += 1;
            let data = peek(buf);
            g.logger.line(&format!(
                "probe: save read {name} ret=0x{ret:x} caller=0x{rva:x} (READ-SIDE) \
                 data={data}{}",
                caller_label(rva)
            ));
        }
        // the periodic histogram
        if g.calls >= g.next_progress {
            g.next_progress = g.calls + PROGRESS_EVERY;
            g.logger.line(&format!(
                "probe: save progress calls={} callers={} sizes: {}",
                g.calls,
                g.callers.len(),
                top_sizes(&g.sizes)
            ));
        }
    }
}

/// Fetch the trampoline at `slot` and log the call. Returns the original's
/// address (the lock is released before it is called: `save_mmf_write_block`
/// calls `mmf_paged_write_bulk` internally, so holding the lock across the
/// forward would deadlock on a re-entered hook).
unsafe fn begin(
    name: &'static str,
    slot: usize,
    ret: usize,
    size: Option<u32>,
    buf: *const u8,
) -> Option<*const c_void> {
    unsafe {
        let m = G.get()?;
        let mut g = m.lock().ok()?;
        note(&mut g, name, ret, size, buf);
        g.tramp[slot].as_ref().map(|t| t.as_ptr())
    }
}

/// The vector's element count at `vec` (`[begin, end)`), bounded by a sanity
/// cap so a bogus pointer cannot spin.
unsafe fn vec_elems(vec: *const *const u32) -> (usize, *const u8) {
    unsafe {
        if vec.is_null() || !sys::is_readable(vec.cast(), 16) {
            return (0, core::ptr::null());
        }
        let begin = *vec;
        let end = *vec.add(1);
        let Some(span) = (end as usize).checked_sub(begin as usize) else {
            return (0, core::ptr::null());
        };
        let n = (span / 4).min(1 << 20);
        if n == 0 || !sys::is_readable(begin.cast(), n * 4) {
            return (0, begin.cast());
        }
        (n, begin.cast())
    }
}

type WriteFn = unsafe extern "system" fn(*mut c_void, *const u8, u32);
type VecFn = unsafe extern "system" fn(u64, *const *const u32);

/// `mmf_paged_write(ring, src, size)` — the byte-level save writer.
unsafe extern "system" fn pw_body(ring: *mut c_void, src: *const u8, size: u32, ret: usize) {
    unsafe {
        let Some(p) = begin(ANCHOR_PW, 0, ret, Some(size), src) else {
            return;
        };
        let f: WriteFn = core::mem::transmute(p);
        f(ring, src, size);
    }
}

/// `mmf_paged_write_bulk(ring, src, size)` — the bulk save writer.
unsafe extern "system" fn pwb_body(ring: *mut c_void, src: *const u8, size: u32, ret: usize) {
    unsafe {
        let Some(p) = begin(ANCHOR_PWB, 1, ret, Some(size), src) else {
            return;
        };
        let f: WriteFn = core::mem::transmute(p);
        f(ring, src, size);
    }
}

/// `save_mmf_write_block(unused, vec)` — the u32-vector block writer. The
/// written size is derived: `(end - begin) / 4` elements, `*4` bytes.
unsafe extern "system" fn wblock_body(arg1: u64, vec: *const *const u32, ret: usize) {
    unsafe {
        let (n, data) = vec_elems(vec);
        let size = (n * 4) as u32;
        let Some(p) = begin(ANCHOR_WBLOCK, 2, ret, Some(size), data) else {
            return;
        };
        let f: VecFn = core::mem::transmute(p);
        f(arg1, vec);
    }
}

/// `mmf_read_u32_vector(unused, vec)` — the READ-side twin. It carries no size
/// argument (it reads a u32 count from the stream), so the pre-call element
/// count is logged instead.
unsafe extern "system" fn rvec_body(arg1: u64, vec: *const *const u32, ret: usize) {
    unsafe {
        let (_, data) = vec_elems(vec);
        let Some(p) = begin(ANCHOR_RVEC, 3, ret, None, data) else {
            return;
        };
        let f: VecFn = core::mem::transmute(p);
        f(arg1, vec);
    }
}
