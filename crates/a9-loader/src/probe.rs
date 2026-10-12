//! The M4 runtime fetch probe — `probe_fetch = true` diagnostics.
//!
//! The question (notes/24 round 3): material fetch classes pass their ctx
//! opaquely through vtable dispatch, so which pack object serves which
//! composed key (e.g. a `train\113\...` livery key) is not statically
//! answerable — it is only visible at runtime. This probe hooks the four
//! fetch-path anchors trampoline-only (no serving behavior change) and logs
//! one line per call: the object pointer + the composed key the call
//! carries.
//!
//! ABI (each machine-verified against the build's own code, not assumed):
//! - `pack_fetch` FUN_140306e20: rcx = pack object, rdx = destination
//!   buffer, r8 = wstring-OBJECT pointer (the composed key; the wide data
//!   lives behind the object's first field — the tree lookup reads `[r8]`).
//! - `texture_fetch` FUN_1403010d0: rcx = ctx (0 = loose-file mode), rdx =
//!   source-path wstr (the mesh's own path), r8 = referenced basename wstr,
//!   r9 low byte = mode. Returns rsi; forwarded verbatim.
//! - `ccommon_fetch` FUN_1402ec0e0: rcx = this, rdx unused (forwarded), r8 =
//!   name (double encoded: a wide pointer or a small id), r9 unused, and
//!   the 5th/6th args are STACK slots = out pointers [buf, size*] (the
//!   cache-hit path stores through them in r12/r13) — they MUST be
//!   forwarded, or a hit writes through garbage.
//! - `fetch_cache_get` FUN_1403089b0: rcx = cache vector object, rdx = raw
//!   key wstr (compared char-by-char as-is). Returns the entry pointer.
//!
//! Every hook returns the original's full result register (all four anchors
//! return pointer-width values; typed as a raw pass-through). Install is
//! in-book order; per-anchor failure logs one loud line and continues
//! vanilla (fail-open, like the overlay).

use crate::{log, patch, sys};
use core::ffi::c_void;

/// The four probe anchors, in install order (builds.toml names).
pub const PROBE_ANCHORS: [&str; 4] = [
    "pack_fetch",
    "texture_fetch",
    "ccommon_fetch",
    "fetch_cache_get",
];

/// One resolved anchor handed over by the engine.
pub struct Spec {
    pub name: &'static str,
    pub rva: usize,
    pub expect: Vec<u8>,
}

struct State {
    logger: log::Logger,
    /// trampolines in `PROBE_ANCHORS` order (None = that anchor's hook
    /// refused at boot and must never be reached)
    trampolines: [Option<patch::Trampoline>; 4],
}

static G: std::sync::OnceLock<State> = std::sync::OnceLock::new();

/// Never read more than this many wide chars for one logged name.
const MAX_NAME_CHARS: usize = 512;

/// Install the probe: expect-verify + trampoline every anchor in order.
/// Returns Err only when there is nothing to install at all; individual
/// anchor refusals log loudly and are skipped (fail-open, like the
/// overlay).
pub unsafe fn install(logger: log::Logger, base: *const u8, specs: &[Spec]) -> Result<(), String> {
    unsafe {
        if specs.is_empty() {
            return Err("no probe anchors recorded for this build".into());
        }
        // Boot runs under the loader lock in DllMain(DLL_PROCESS_ATTACH):
        // no game thread exists yet, so publishing G before the first hook
        // write guarantees every installed hook sees a populated G.
        let _ = G.set(State {
            logger,
            trampolines: [None, None, None, None],
        });
        let logger = &G.get().unwrap().logger;
        let tramp_head =
            &G.get().unwrap().trampolines as *const Option<patch::Trampoline>
                as *mut Option<patch::Trampoline>;
        let mut installed = 0usize;
        for spec in specs {
            let slot = match PROBE_ANCHORS.iter().position(|n| *n == spec.name) {
                Some(i) => i,
                None => continue,
            };
            let hook: *const c_void = match spec.name {
                "pack_fetch" => my_pack_fetch as *const c_void,
                "texture_fetch" => my_texture_fetch as *const c_void,
                "ccommon_fetch" => my_ccommon_fetch as *const c_void,
                "fetch_cache_get" => my_cache_get as *const c_void,
                _ => continue,
            };
            let entry = base.cast_mut().add(spec.rva);
            match patch::install_trampoline(
                entry,
                hook,
                &spec.expect,
                sys::PAGE_EXECUTE_READWRITE,
            ) {
                Ok(tramp) => {
                    logger.line(&format!(
                        "probe: {} hook installed rva={:#x} expect=verified",
                        spec.name, spec.rva
                    ));
                    // the trampoline is needed at the hook's first call;
                    // publish through the raw head (boot-only writer,
                    // read-only afterwards)
                    *tramp_head.add(slot) = Some(tramp);
                    installed += 1;
                }
                Err(e) => {
                    logger.err(&format!(
                        "probe refused: {} rva={:#x}: {e}",
                        spec.name, spec.rva
                    ));
                }
            }
        }
        if installed == 0 {
            return Err("all probe anchors refused".into());
        }
        Ok(())
    }
}

/// Read a wide string at `p` (bounded), requiring its pages readable.
fn wide_str(p: *const u16) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let mut units: Vec<u16> = Vec::with_capacity(64);
    let mut q = p;
    unsafe {
        if !sys::is_readable(p.cast(), 32) {
            return None;
        }
        loop {
            let u = *q;
            if u == 0 {
                break;
            }
            units.push(u);
            if units.len() >= MAX_NAME_CHARS {
                return None; // runaway string — refuse rather than guess
            }
            q = q.add(1);
            if units.len() & 0x1f == 0 && !sys::is_readable(q.cast(), 32) {
                return None;
            }
        }
    }
    if units.is_empty() {
        return None;
    }
    Some(String::from_utf16_lossy(&units))
}

/// Wide NAME behind a possibly-double-encoded pointer: either the raw wstr
/// itself (readable, sane) or a wstring-object whose first field is the
/// data pointer. Decides by head sanity: a raw path's first wide unit is
/// printable ASCII; for an object pointer the first unit is the data
/// pointer's low word (its high byte is rarely 0 and the unit pair almost
/// never decodes as a long printable run), so the bounded read + one
/// printable check separates the encodings well enough for a diagnostic.
fn wide_str_indirect(p: *const c_void) -> Option<String> {
    if p.is_null() {
        return None;
    }
    if let Some(s) = wide_str(p.cast())
        && sane_head(&s)
    {
        return Some(s);
    }
    unsafe {
        let obj = p as *const *const u16;
        if sys::is_readable(obj.cast(), 16)
            && let Some(s) = wide_str(*obj)
            && sane_head(&s)
        {
            return Some(s);
        }
    }
    None
}

/// First logged char must be printable ASCII (game keys are ASCII paths).
fn sane_head(s: &str) -> bool {
    s.chars().next().is_some_and(|c| ('\u{20}'..='\u{7e}').contains(&c))
}

type PackFetchFn = unsafe extern "system" fn(
    pack: *mut c_void,
    dest: *mut c_void,
    name_obj: *const c_void,
) -> *mut c_void;

/// The pack-object fetch: `probe: pack_fetch obj=%p name=%ls dest=%p`.
unsafe extern "system" fn my_pack_fetch(
    pack: *mut c_void,
    dest: *mut c_void,
    name_obj: *const c_void,
) -> *mut c_void {
    unsafe {
        let Some(state) = G.get() else {
            return core::ptr::null_mut(); // cannot be reached after install
        };
        let name = wide_str_indirect(name_obj).unwrap_or_else(|| "<unreadable>".into());
        state.logger.line(&format!(
            "probe: pack_fetch obj={pack:p} name={name:?} dest={dest:p}"
        ));
        let original: PackFetchFn = match state.trampolines[0].as_ref() {
            Some(t) => core::mem::transmute(t.as_ptr()),
            None => return core::ptr::null_mut(),
        };
        original(pack, dest, name_obj)
    }
}

type TextureFetchFn = unsafe extern "system" fn(
    ctx: *mut c_void,
    src_path: *const u16,
    name: *const u16,
    mode: u32,
) -> *mut c_void;

/// The texture fetch-miss path:
/// `probe: texture_fetch ctx=%p src=%ls name=%ls mode=%u`.
unsafe extern "system" fn my_texture_fetch(
    ctx: *mut c_void,
    src_path: *const u16,
    name: *const u16,
    mode: u32,
) -> *mut c_void {
    unsafe {
        let Some(state) = G.get() else {
            return core::ptr::null_mut();
        };
        // src: the notes/14 chain splits the src's dir with _wsplitpath_s —
        // the arg is a good wide pointer but NOT always a bare string start
        // (live run showed "<unreadable>" for the plain read); try the
        // indirect heuristic too, and log the raw pointer when both fail so
        // the ambiguity is visible instead of hidden
        let src = wide_str_indirect(src_path.cast())
            .unwrap_or_else(|| format!("<unreadable {src_path:p}>"));
        let refname = wide_str_indirect(name.cast())
            .unwrap_or_else(|| format!("<unreadable {name:p}>"));
        state.logger.line(&format!(
            "probe: texture_fetch ctx={ctx:p} src={src:?} name={refname:?} mode={}",
            mode & 0xff
        ));
        let original: TextureFetchFn = match state.trampolines[1].as_ref() {
            Some(t) => core::mem::transmute(t.as_ptr()),
            None => return core::ptr::null_mut(),
        };
        original(ctx, src_path, name, mode)
    }
}

type CCommonFetchFn = unsafe extern "system" fn(
    this: *mut c_void,
    arg2: usize,
    name: *const u16,
    arg4: usize,
    buf_out: *mut *mut c_void,
    size_out: *mut u32,
) -> *mut c_void;

/// The CCommonShader fetch:
/// `probe: ccommon_fetch mgr=%p name=%ls` — the 5th/6th (stack-slot) out
/// pointers are forwarded untouched; the name is read behind the object.
unsafe extern "system" fn my_ccommon_fetch(
    this: *mut c_void,
    arg2: usize,
    name: *const u16,
    arg4: usize,
    buf_out: *mut *mut c_void,
    size_out: *mut u32,
) -> *mut c_void {
    unsafe {
        let Some(state) = G.get() else {
            return core::ptr::null_mut();
        };
        let refname = wide_str_indirect(name.cast())
            .unwrap_or_else(|| "<unreadable>".into());
        state
            .logger
            .line(&format!("probe: ccommon_fetch mgr={this:p} name={refname:?}"));
        let original: CCommonFetchFn = match state.trampolines[2].as_ref() {
            Some(t) => core::mem::transmute(t.as_ptr()),
            None => return core::ptr::null_mut(),
        };
        original(this, arg2, name, arg4, buf_out, size_out)
    }
}

type CacheGetFn =
    unsafe extern "system" fn(cache: *mut c_void, name: *const u16) -> *mut c_void;

/// The global fetch-cache get: `probe: cache_get cache=%p name=%ls`.
unsafe extern "system" fn my_cache_get(
    cache: *mut c_void,
    name: *const u16,
) -> *mut c_void {
    unsafe {
        let Some(state) = G.get() else {
            return core::ptr::null_mut();
        };
        let refname = wide_str(name).unwrap_or_else(|| "<unreadable>".into());
        state
            .logger
            .line(&format!("probe: cache_get cache={cache:p} name={refname:?}"));
        let original: CacheGetFn = match state.trampolines[3].as_ref() {
            Some(t) => core::mem::transmute(t.as_ptr()),
            None => return core::ptr::null_mut(),
        };
        original(cache, name)
    }
}
