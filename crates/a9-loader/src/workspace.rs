//! M5-T — the family-A decode workspace swap (removing the 200 MB ceiling).
//!
//! The shared decode workspace is a boot-time singleton (`DAT_144f86088`):
//!
//! ```text
//! +0x00  qword  OUT buffer pointer   (allocated once, 200 MB)
//! +0x08  qword  IN  buffer pointer   (allocated once, 150 MB)
//! +0x10  u32    OUT cap = 0xC800000  (the LZSS decoder's output bound)
//! +0x14  u32    IN  cap = 0x9600000  (never read anywhere)
//! ```
//!
//! `boot_pack_system_driver` (0x140319480) sets the caps, calls the game's own
//! `_aligned_malloc` twice, and then calls `pack_master_map_access`. That last
//! call is our seam: both buffers exist and are non-NULL, and every consumer
//! (and the teardown) re-reads the pointer from the global, so replacing the
//! OUT buffer there is transparent.
//!
//! Why the *alloc* seam and not a post-decode copy: the decoder is bounded by
//! the OUT **cap**, so a pack that decodes to more than the cap has already
//! failed by the time any post-decode hook runs. Only a bigger buffer with a
//! bigger cap, in place before the decode, lifts the ceiling.
//!
//! The new buffer must come from the game's own `_aligned_malloc`
//! (0x140597ba8) — the teardown frees it with `_aligned_free`, which expects
//! that allocator's header — and the OUT cap must be raised with it. The IN
//! buffer is left alone: its cap is never read and the tailgate never touches
//! the encoded file, so it is not a limiter for this path.
//!
//! Every failure path is fail-open: log one loud line and leave the game's
//! buffers and caps exactly as the game set them.

use crate::{log, patch, sys};
use a9_mod::builds::BuildEntry;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// `_aligned_malloc(size, align) -> ptr`
type AlignedMalloc = unsafe extern "system" fn(usize, usize) -> *mut u8;
/// `_aligned_free(ptr)`
type AlignedFree = unsafe extern "system" fn(*mut u8);

/// the shipped caps the game writes at the singleton — the runtime fingerprint
/// that proves the address we hold really is the workspace struct
const SHIPPED_OUT_CAP: u32 = 0xC800000; // 200 MiB
const SHIPPED_IN_CAP: u32 = 0x9600000; // 150 MiB

const OFF_OUT_PTR: usize = 0x00;
const OFF_OUT_CAP: usize = 0x10;

struct Inner {
    logger: log::Logger,
    /// `base + workspace_globals`
    globals: usize,
    /// `base + aligned_malloc` / `base + aligned_free`
    malloc: usize,
    free: usize,
    /// the requested OUT buffer size in bytes
    target_out: usize,
}

static G: OnceLock<Inner> = OnceLock::new();

/// The site's pinned call target, read by the naked shim's tail jump. A plain
/// global so the asm can reach it with a RIP-relative load.
#[unsafe(no_mangle)]
pub static A9_WS_ORIG: AtomicU64 = AtomicU64::new(0);

/// Read `len` bytes at `addr`, refusing (never faulting) on unmapped memory.
unsafe fn readable(addr: usize, len: usize) -> Option<&'static [u8]> {
    let p = addr as *const c_void;
    if !unsafe { sys::is_readable(p, len) } {
        return None;
    }
    Some(unsafe { core::slice::from_raw_parts(p as *const u8, len) })
}

// The naked entry shim: the relay cave jumps here (the game's `E8` pushed the
// continuation, so `RSP ≡ 8 (mod 16)` at entry, exactly like a real call). It
// preserves the caller's four argument registers, runs the Rust stub, and
// TAIL-JUMPS to the original target — so the callee sees the caller's stack,
// its own arguments (register *and* stack) and returns straight to the game's
// continuation, with RAX flowing back untouched.
//
// Stack: 4 pushes (32 B, a multiple of 16) keep `RSP ≡ 8`; `sub rsp,0x28`
// (shadow space + alignment) makes it `≡ 0` before the `call`, so the stub
// enters with the standard alignment. Everything is unwound before the jump.
core::arch::global_asm!(
    ".globl a9_ws_shim",
    ".p2align 4",
    "a9_ws_shim:",
    "push rcx",
    "push rdx",
    "push r8",
    "push r9",
    "sub rsp, 0x28",
    "call {stub}",
    "add rsp, 0x28",
    "pop r9",
    "pop r8",
    "pop rdx",
    "pop rcx",
    "jmp qword ptr [rip + {orig}]",
    stub = sym workspace_stub,
    orig = sym A9_WS_ORIG,
);

unsafe extern "C" {
    fn a9_ws_shim();
}

fn shim_addr() -> *const c_void {
    let f: unsafe extern "C" fn() = a9_ws_shim;
    f as *const c_void
}

/// The spliced alloc-tail wrapper. Total: every failure path logs and leaves
/// the game's workspace exactly as it is.
unsafe extern "system" fn workspace_stub() {
    unsafe {
        let Some(inner) = G.get() else {
            return;
        };
        let g = inner.globals;
        let Some(caps) = readable(g + OFF_OUT_CAP, 8) else {
            inner.logger.err("M5-T workspace: the caps are not readable — not swapping");
            return;
        };
        let out_cap = u32::from_le_bytes([caps[0], caps[1], caps[2], caps[3]]);
        let in_cap = u32::from_le_bytes([caps[4], caps[5], caps[6], caps[7]]);
        if out_cap != SHIPPED_OUT_CAP || in_cap != SHIPPED_IN_CAP {
            inner.logger.err(&format!(
                "M5-T workspace: caps at {g:#x} read {out_cap:#x}/{in_cap:#x}, not the shipped \
                 {SHIPPED_OUT_CAP:#x}/{SHIPPED_IN_CAP:#x} — the workspace was already sized by \
                 something else; NOT swapping"
            ));
            return;
        }
        let malloc: AlignedMalloc = core::mem::transmute(inner.malloc);
        let free: AlignedFree = core::mem::transmute(inner.free);
        let new = malloc(inner.target_out, 0x10);
        if new.is_null() {
            inner.logger.err(&format!(
                "M5-T workspace: _aligned_malloc({} MiB) failed — keeping the game's {} MiB buffer \
                 (large family-A mod sets may refuse their payloads)",
                inner.target_out >> 20,
                SHIPPED_OUT_CAP >> 20
            ));
            return;
        }
        let Some(slot) = readable(g + OFF_OUT_PTR, 8) else {
            inner.logger.err("M5-T workspace: the OUT pointer is not readable — not swapping");
            free(new);
            return;
        };
        let old = usize::from_le_bytes([
            slot[0], slot[1], slot[2], slot[3], slot[4], slot[5], slot[6], slot[7],
        ]);
        // publish the new buffer + cap first, then release the game's
        core::ptr::write_unaligned((g + OFF_OUT_PTR) as *mut usize, new as usize);
        core::ptr::write_unaligned((g + OFF_OUT_CAP) as *mut u32, inner.target_out as u32);
        if old != 0 {
            free(old as *mut u8);
        }
        inner.logger.line(&format!(
            "M5-T workspace: OUT buffer {} MiB -> {} MiB at {new:p} (game's own _aligned_malloc, \
             cap raised; the game's teardown frees it)",
            SHIPPED_OUT_CAP >> 20,
            inner.target_out >> 20
        ));
    }
}

/// Arm the swap (boot time): resolve + verify the anchors, stash the runtime
/// addresses, install the relay cave and splice the alloc tail.
///
/// `target_out` is the OUT buffer size in bytes the caller decided on.
pub unsafe fn install(
    entry: &BuildEntry,
    base: *const u8,
    target_out: usize,
    logger: log::Logger,
) -> Result<String, String> {
    let anchor = |n: &str| {
        entry
            .anchor(n)
            .ok_or_else(|| format!("this build has no {n} anchor"))
    };
    let site = anchor("workspace_alloc_tail")?;
    let callee = anchor("workspace_alloc_callee")?;
    let globals = anchor("workspace_globals")?;
    let malloc = anchor("aligned_malloc")?;
    let free = anchor("aligned_free")?;
    let cave = anchor("workspace_cave")?;

    let site_expect = site.expect_bytes().unwrap_or_default();
    let cave_expect = cave.expect_bytes().unwrap_or_default();
    // The two CRT functions we call by address (`_aligned_malloc` /
    // `_aligned_free`) are NOT byte-verified here: they are only reachable at
    // runtime and their entries carry no distinguishing prologue worth pinning
    // (the offline `a9ml gen-builds --check` verifies those bytes against the
    // exe, and the build gate guards the address book as a whole). The site,
    // the cave and the site's pinned target — everything this splice writes —
    // are verified.

    let site_ptr = unsafe { base.add(site.rva as usize) } as *mut u8;
    let cave_ptr = unsafe { base.add(cave.rva as usize) } as *mut u8;
    let target = base as usize + callee.rva as usize;
    unsafe {
        patch::install_relay_cave(cave_ptr, &cave_expect, shim_addr(), sys::PAGE_EXECUTE_READWRITE)?;
        let orig = patch::splice_to_cave(
            site_ptr,
            &site_expect,
            cave_ptr,
            sys::PAGE_EXECUTE_READWRITE,
            Some(target),
        )?;
        A9_WS_ORIG.store(orig as u64, Ordering::Release);
    }
    let _ = G.set(Inner {
        logger,
        globals: base as usize + globals.rva as usize,
        malloc: base as usize + malloc.rva as usize,
        free: base as usize + free.rva as usize,
        target_out,
    });
    Ok(format!(
        "M5-T workspace: swap armed at rva={:#x} (the alloc tail, expect=verified) — OUT {} MiB \
         via the game's _aligned_malloc {:#x}",
        site.rva,
        target_out >> 20,
        malloc.rva
    ))
}

/// The OUT buffer size to ask for: `0` = keep the game's buffer.
///
/// AUTO (config `workspace_out_mb = 0`) grows only when the planned family-A
/// content does not fit the shipped cap, with headroom for the game's own use.
/// A positive config value forces that size — the "as much as the machine can
/// take" soft limit. Clamped to what the `u32` cap field can express.
pub fn target_size(cfg_mb: u32, needed: usize, logger: &log::Logger) -> usize {
    const SHIPPED: usize = SHIPPED_OUT_CAP as usize;
    const SLACK: usize = 64 * 1024 * 1024;
    /// the cap field is a `u32`: 3.5 GiB is the largest sane request
    const MAX: usize = 3584 * 1024 * 1024;
    let mut want = if cfg_mb > 0 {
        (cfg_mb as usize) << 20
    } else {
        0
    };
    if needed > SHIPPED {
        // the plan itself needs more than the shipped buffer
        let total = needed.saturating_add(SLACK);
        if want < total {
            if want > 0 {
                logger.line(&format!(
                    "M5-T workspace: the requested {} MiB does not cover the planned {} MiB of \
                     family-A content — growing to {} MiB",
                    want >> 20,
                    needed >> 20,
                    total >> 20
                ));
            }
            want = total;
        }
    }
    if want <= SHIPPED {
        // fits (or nothing was asked for): leave the game's allocation alone
        return 0;
    }
    if want > MAX {
        logger.line(&format!(
            "M5-T workspace: requested {} MiB is beyond the u32 cap field — clamping to {} MiB",
            want >> 20,
            MAX >> 20
        ));
    }
    want.min(MAX)
}
