//! The M5 family-A **tailgate** — disk-free serving of `[[asset]]` new
//! assets. Instead of decode → append → re-encode a host pack into a cache
//! file and redirect its open (the M5 W1 merge), the tailgate splices the
//! game's ONE family-A LZSS decode funnel and appends our bytes into the
//! shared workspace's decoded image **right after the decode**, so the
//! consumers (which run immediately after in the same boot window) see them.
//! No re-encode, no cache file, nothing on disk.
//!
//! # Machine facts this stub is built on (RE doc `/tmp/opencode/m5-round3/family-a-serving.md`)
//!
//! * the funnel is `pack_lzss_decode` (builds.toml `pack_lzss_decode`,
//!   ABI `RCX=dest, RDX=&u32 cap-in/len-out, R8=src (inBuf+0x14), R9D=srclen`,
//!   returns `AL=1` on success and writes the decoded length back to `*RDX`);
//! * the family-A loaders we serve (`vp`, `tp`, `mcp`, `ep`, `cp`, `i.dat`)
//!   each reach it through one `E8` (the `pack_lzss_*_call` sites in
//!   builds.toml); the splice rewrites that `E8`'s rel32 to a same-image relay
//!   cave (one cave, `FF 25` + imm64) that jumps into our dll — exactly the M4
//!   famb splice, via [`crate::patch`];
//! * nothing validates the decoded length against the declared-size word
//!   (`inBuf+0x10`), nothing re-scans, and no loader ever reads that word — so
//!   bytes appended past `|V|` (still under the 200 MB out-cap) are invisible
//!   to the game;
//! * the workspace is reused per pack (the next decode overwrites from offset
//!   0), so appended bytes are valid only inside their host's window — which
//!   is exactly when the consumers run.
//!
//! # Dispatch
//!
//! One cave serves every site, so the stub identifies the host by the
//! caller's continuation (`[RSP]` at entry = `site+5`). A naked shim captures
//! that value and passes it as a 5th argument to the Rust stub, which
//! dispatches on it; the shim also keeps the Win64 stack discipline so the
//! stub's `ret` returns to `site+5` exactly like the original `E8` would.
//!
//! # Append
//!
//! * `vp`/`tp` (and any family-A host): the payloads are appended at
//!   `dest+|V|` in planned order, where `|V|` is the vanilla **declared** size
//!   read from the pack's own header at boot — the same value the synthesized
//!   `i.dat` rows carry as their `off`;
//! * `i.dat`: the synthesized entry records are appended at `dest+|V_idat|`
//!   and the `u32` entry count at `dest+0x18` is bumped by the row count.
//!
//! Every failure path is total and fail-open: log ONE loud line and leave the
//! decode's own result untouched (the game keeps serving vanilla, or the
//! merge fallback covers the host).
//!
//! # Declared size vs the runtime decoded length
//!
//! The game's decoder decodes the staged input to EXHAUSTION, so it normally
//! produces a few bytes MORE than the declared size (measured live: vp +119,
//! tp +45, i.dat +21 — a deterministic per-file delta, memory #312). Those
//! trailing bytes are decoder junk: they sit past every i.dat row's
//! `(off, len)` window (the M1 row sweep proved every row fits inside the
//! declared size), so no consumer ever reads them. The stub therefore appends
//! at the **declared** base — which is exactly what the rows say — and simply
//! overwrites that junk (it logs the delta). Appending at the runtime end
//! instead would need the rows' offsets patched into the master map after the
//! fact; using the declared base keeps the plan static and needs no game
//! function calls at all.
//!
//! A decode SHORTER than the declared size is the unexpected case (the tail
//! could reach past what the loader produced), so the stub refuses to append,
//! latches the host, logs loudly, and returns the decode's own `AL`.
//!
//! The `i.dat` rows cannot be retracted: `i.dat` is a STAGE-1 pack (it decodes
//! before any stage-2 host), so by the time a host's problem is seen its rows
//! are already in the master map. The engine therefore tailgates `i.dat` ONLY
//! when every host tailgates — if any host is on the merge path the rows come
//! from the merged index, whose atomicity refuses the whole index when a host
//! cache fails, so a runtime-appended row can never point at a host that ended
//! up vanilla.

use crate::{log, patch, sys};
use core::ffi::c_void;
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// `pack_lzss_decode` ABI: `(dest, &cap, src, srclen) -> AL`.
type DecodeFn = unsafe extern "system" fn(*mut u8, *mut u32, *const u8, u32) -> u8;

/// `DAT_144f86088` — the shared decode workspace's OUT pointer (200 MB). The
/// loaders re-read it right before each decode; the stub checks the `dest` it
/// was handed is that workspace before writing.
const WORKSPACE_OUT_RVA: usize = 0x4F86088;

/// the `u32` entry-count word inside a decoded `i.dat` image (the loader
/// reads it at `out+0x18` and starts its entry cursor at `out+0x1c`)
const IDAT_COUNT_OFF: usize = 0x18;

/// One host's boot-materialized append plan, keyed at runtime by the site's
/// continuation address.
struct HostPlan {
    /// pack key for the loud lines ("vp"/"tp"/"i.dat")
    key: String,
    /// the appended-at offset: the vanilla declared size (== decoded length)
    declared: usize,
    /// the bytes to append, in order (host payloads, or one rendered i.dat
    /// row blob)
    parts: Vec<Vec<u8>>,
    /// `i.dat` only: the entry-count bump (0 for a host pack)
    idat_rows: u32,
    /// Σ|parts|
    total: usize,
    /// set once when the declared-vs-actual assertion fails: loud-once, and no
    /// further append is attempted for this host
    failed: AtomicBool,
}

struct Inner {
    /// `base + pack_lzss_decode` — the stub's call target
    decode: usize,
    /// site continuation (`base + site_rva + 5`) → plan
    hosts: BTreeMap<usize, HostPlan>,
    logger: log::Logger,
}

static G: OnceLock<Inner> = OnceLock::new();

/// One site the engine asks the tailgate to splice: the pack key, the `E8`
/// site (RVA + expect bytes), and everything the stub must append there.
pub struct SitePlan {
    pub key: String,
    pub site_rva: usize,
    pub site_expect: Vec<u8>,
    /// the vanilla declared size (the append base == the `i.dat` row offset)
    pub declared: usize,
    /// payloads in planned order (host) — the row blob for `i.dat`
    pub parts: Vec<Vec<u8>>,
    /// `i.dat` only: the entry-count bump (0 for a host pack)
    pub idat_rows: u32,
}

/// The whole tailgate request: the funnel address, the shared relay cave, and
/// one [`SitePlan`] per host to serve.
pub struct InstallPlan {
    /// `pack_lzss_decode` RVA (the stub's call target; also the pinned
    /// original target of every site's `E8`)
    pub decode_rva: usize,
    pub cave_rva: usize,
    pub cave_expect: Vec<u8>,
    pub sites: Vec<SitePlan>,
}

/// Install the tailgate: write the shared relay cave once, splice every site
/// to it, publish the runtime plan. Returns the pack keys actually spliced
/// (in request order) — a site that refuses (missing expect bytes / book
/// drift) is skipped with a loud line and falls back to merge. Only a cave
/// failure is fatal (nothing can be spliced then).
pub unsafe fn install(
    plan: InstallPlan,
    base: *const u8,
    logger: log::Logger,
) -> Result<Vec<String>, String> {
    unsafe {
        let cave = base.add(plan.cave_rva) as *mut u8;
        // one cave serves every site (the stub dispatches on the caller's
        // continuation), so install it ONCE — a second install_relay_cave
        // would compare the already-written thunk against the CC expect bytes
        // and refuse.
        patch::install_relay_cave(
            cave,
            &plan.cave_expect,
            shim_addr(),
            sys::PAGE_EXECUTE_READWRITE,
        )?;
        let decode = base.add(plan.decode_rva) as usize;
        let mut hosts = BTreeMap::new();
        let mut spliced = Vec::new();
        for s in plan.sites {
            let site = base.add(s.site_rva) as *mut u8;
            match patch::splice_to_cave(
                site,
                &s.site_expect,
                cave,
                sys::PAGE_EXECUTE_READWRITE,
                Some(decode),
            ) {
                Ok(original) => {
                    logger.line(&format!(
                        "asset tailgate: {} splice installed rva={:#x} expect=verified \
                         (original target {original:#x} = pack_lzss_decode)",
                        s.key, s.site_rva
                    ));
                    let total = s.parts.iter().map(|p| p.len()).sum::<usize>();
                    let site_cont = base as usize + s.site_rva + 5;
                    hosts.insert(
                        site_cont,
                        HostPlan {
                            key: s.key.clone(),
                            declared: s.declared,
                            parts: s.parts,
                            idat_rows: s.idat_rows,
                            total,
                            failed: AtomicBool::new(false),
                        },
                    );
                    spliced.push(s.key);
                }
                Err(e) => logger.err(&format!(
                    "asset tailgate: {} splice refused rva={:#x}: {e} — this \
                     host falls back to merge serving",
                    s.key, s.site_rva
                )),
            }
        }
        let _ = G.set(Inner {
            decode,
            hosts,
            logger,
        });
        Ok(spliced)
    }
}

// The naked entry shim: the relay cave jumps here (the game's `E8` pushed
// `site+5`). It reads that continuation into R10, sets it up as the 5th Win64
// argument for [`tailgate_stub`], and calls the stub with the caller's
// argument registers intact.
//
// Stack: at entry `RSP ≡ 8 (mod 16)` (the game called its `E8` from a
// 16-aligned RSP; the relay `jmp` pushes nothing). `sub rsp,0x38` → `RSP ≡ 0`
// before the `call` (0x38 = 0x20 shadow + the 5th-arg slot at +0x20 + 16
// padding), so the callee sees the standard alignment and its `ret` returns
// to `site+5` with the stack exactly as the original call left it. AL (the
// stub's return) survives `add rsp`/`ret` untouched.
core::arch::global_asm!(
    ".globl a9_tailgate_shim",
    ".p2align 4",
    "a9_tailgate_shim:",
    "mov r10, [rsp]",       // r10 = site+5 (the game's continuation)
    "sub rsp, 0x38",
    "mov [rsp+0x20], r10",  // Win64 arg5 slot for the upcoming call
    "call {stub}",
    "add rsp, 0x38",
    "ret",
    stub = sym tailgate_stub,
);

unsafe extern "C" {
    fn a9_tailgate_shim();
}

fn shim_addr() -> *const c_void {
    let f: unsafe extern "C" fn() = a9_tailgate_shim;
    f as *const c_void
}

/// The spliced decode wrapper. Calls the original decoder with the caller's
/// exact arguments, then — only on a clean host match and a clean
/// declared-vs-actual match — appends the host's bytes into the workspace and
/// returns the decoder's own `AL`. Total: every failure path logs and returns
/// the untouched result.
unsafe extern "system" fn tailgate_stub(
    dest: *mut u8,
    cap: *mut u32,
    src: *const u8,
    srclen: u32,
    site: usize,
) -> u8 {
    unsafe {
        let Some(inner) = G.get() else {
            // unreachable: the caves only ever point at an installed stub
            return 0;
        };
        let decode: DecodeFn = core::mem::transmute(inner.decode);
        let al = decode(dest, cap, src, srclen);
        if al == 0 {
            return al; // cap hit — nothing was decoded, nothing to append
        }
        let Some(hp) = inner.hosts.get(&site) else {
            return al; // not one of our sites (cannot happen)
        };
        if hp.failed.load(Ordering::Relaxed) {
            return al;
        }
        // the decoded length is now in *cap; the plan used the vanilla
        // declared size. They must agree or the i.dat rows would dangle.
        if cap.is_null() {
            return hard_fail(inner, hp, "the decoder returned a null capacity pointer", al);
        }
        let decoded = core::ptr::read(cap) as usize;
        // The plan's append base is the vanilla DECLARED size. The game's
        // decoder decodes the staged input to EXHAUSTION, so it normally
        // produces a few bytes MORE than declared (live vp +119, tp +45,
        // i.dat +21 — memory #312): those extra bytes are decoder tail junk
        // that sits past every i.dat row's (off, len) window (the M1 row
        // sweep proved every row fits inside `declared`), so nothing reads
        // them and the append simply overwrites them. A decode SHORTER than
        // the plan is the unexpected case — refuse, the tail could reach past
        // what the loader produced.
        if decoded < hp.declared {
            return hard_fail(
                inner,
                hp,
                &format!(
                    "the decoder produced {decoded} bytes but the plan appends at the \
                     vanilla declared size {} — refusing (the tail could reach past the \
                     decoded content); NOT appending",
                    hp.declared
                ),
                al,
            );
        }
        if decoded > hp.declared {
            inner.logger.line(&format!(
                "asset tailgate: {} decoded {decoded} B — {} B past the declared size {} \
                 (the decoder's tail junk, overwritten by the append; no i.dat row \
                 references it)",
                hp.key,
                decoded - hp.declared,
                hp.declared
            ));
        }
        // the destination must be the shared workspace (the loaders read it
        // fresh right before the call; every consumer adds `off` to it)
        let ws = sys::GetModuleHandleW(core::ptr::null()).cast::<u8>();
        let ws_out = core::ptr::read(ws.add(WORKSPACE_OUT_RVA) as *const *mut u8);
        if ws_out.is_null() || ws_out != dest {
            return hard_fail(
                inner,
                hp,
                "decode destination is not the shared workspace out buffer",
                al,
            );
        }
        // the whole [0, declared+Σ) range must be live before we write the tail
        if !sys::is_readable(dest.cast(), hp.declared + hp.total) {
            return hard_fail(inner, hp, "workspace tail is not readable/writable", al);
        }
        if src.is_null() || !sys::is_readable(src.sub(4).cast(), 4) {
            return hard_fail(inner, hp, "the declared-size word is unreadable", al);
        }
        if hp.idat_rows > 0 && decoded < IDAT_COUNT_OFF + 4 {
            return hard_fail(
                inner,
                hp,
                "decoded i.dat image is too short to hold the entry count",
                al,
            );
        }

        // commit: append the parts at dest+|V|, then (i.dat) bump the count
        let mut off = hp.declared;
        for part in &hp.parts {
            core::ptr::copy_nonoverlapping(part.as_ptr(), dest.add(off), part.len());
            off += part.len();
        }
        if hp.idat_rows > 0 {
            let img = core::slice::from_raw_parts_mut(dest, decoded);
            if let Err(e) = a9_formats::idat::bump_count(img, hp.idat_rows) {
                // cannot happen (length checked above); loud if it ever does
                return hard_fail(inner, hp, &format!("i.dat count bump: {e}"), al);
            }
        }
        inner.logger.line(&format!(
            "asset tailgate: {} appended {} payload{} ({} B) at +{} (workspace, \
             no re-encode)",
            hp.key,
            hp.parts.len(),
            if hp.parts.len() == 1 { "" } else { "s" },
            hp.total,
            hp.declared
        ));
        al
    }
}

/// Loud-once hard failure: name the anchor/asset, latch the plan so the
/// condition is not re-logged and no append is ever attempted for it, and
/// return the decoder's original `AL` untouched (fail-open).
fn hard_fail(inner: &Inner, hp: &HostPlan, why: &str, al: u8) -> u8 {
    if !hp.failed.swap(true, Ordering::Relaxed) {
        inner.logger.err(&format!(
            "asset tailgate: {} FAILED at the decode stub: {why} — serving vanilla \
             (no append this boot)",
            hp.key
        ));
    }
    al
}
