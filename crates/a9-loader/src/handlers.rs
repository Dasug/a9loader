//! Effect handlers — the only code that runs in the game's own call path
//! (`my_filter`), plus the boot-time effect executors. Handlers are total:
//! no panics, no unwinding; every failure is a `Err(String)` the engine logs
//! while the game continues untouched.
//!
//! M0's filter machinery lives on here as the `train_list` handler; the M2
//! additions are the deny list mode, anchor/expect verification for every
//! write, and readability guards for raw-RVA targets (a typo'd rva refuses
//! loudly instead of crashing the game with an access violation).

use crate::{forms, log, patch, sys};
use a9_mod::builds::BuildEntry;
use a9_mod::hex;
use a9_mod::manifest::{Hook, ListMode, Patch, TargetRef};
use core::ffi::c_void;
use std::path::Path;
use std::sync::OnceLock;

/// The C scans at most 16384 UTF-16 units of the FORM list (M0 parity).
const FORMS_SCAN_LIMIT: usize = 16384;
const FORMS_MAX: usize = forms::FORMS_MAX;

static G_ALLOWED: OnceLock<Box<[u8; FORMS_MAX / 8]>> = OnceLock::new();
/// per-call trace logger for the installed filter (the list windows call it
/// on every rebuild — the trace shows the INPUT list each time, which is
/// the observable of "trains disappear after cycling the type filter":
/// shrinking inputs = rows being invalidated between rebuilds)
static G_TRACE: OnceLock<log::Logger> = OnceLock::new();

type FilterFn =
    unsafe extern "system" fn(*mut c_void, *mut u32, *mut u32, *mut c_void) -> *mut c_void;

/// The filter replacement — identical to the C `my_filter`: keep only ids
/// with a bitmap bit, write the new end through `*out`, return `out`.
/// Total: no panics, no allocation, raw pointers only.
unsafe extern "system" fn my_filter(
    out: *mut c_void,
    begin: *mut u32,
    end: *mut u32,
    _extra: *mut c_void,
) -> *mut c_void {
    unsafe {
        let allowed = G_ALLOWED.get().map(|b| &**b);
        let mut w = begin;
        let mut p = begin;
        let mut dropped: Vec<u32> = Vec::new();
        while p != end {
            let id = *p;
            let mut keep = allowed.is_none(); // no bitmap = keep (nop-ish)
            if let Some(a) = allowed {
                let i = id as usize;
                if i < FORMS_MAX {
                    keep = a[i >> 3] & (1u8 << (i & 7)) != 0;
                } else {
                    keep = false;
                }
            }
            if keep {
                *w = id;
                w = w.add(1);
            } else {
                dropped.push(id);
            }
            p = p.add(1);
        }
        *(out as *mut *mut u32) = w;
        if let Some(t) = G_TRACE.get() {
            let n_drop = dropped.len();
            let sample: Vec<String> =
                dropped.iter().take(12).map(|d| format!("{d}")).collect();
            t.line(&format!(
                "train_list filter: in={} kept={} dropped={}{}",
                (end as usize - begin as usize) / 4,
                (w as usize - begin as usize) / 4,
                n_drop,
                if n_drop > 0 {
                    format!(" [{}{}]", sample.join(","), if n_drop > 12 { ",…" } else { "" })
                } else {
                    String::new()
                }
            ));
        }
        out
    }
}

// ------------------------------------------------------------------ targets

struct ResolvedTarget {
    rva: usize,
    /// original bytes recorded in the address book (raw targets carry their
    /// own); empty = nothing recorded to verify
    expect: Vec<u8>,
}

fn describe(t: &TargetRef) -> String {
    match t {
        TargetRef::Anchor(a) => a.clone(),
        TargetRef::Raw { rva, .. } => format!("raw:{rva:#x}"),
    }
}

fn resolve_target(t: &TargetRef, entry: &BuildEntry) -> Result<ResolvedTarget, String> {
    match t {
        TargetRef::Anchor(name) => {
            let a = entry
                .anchor(name)
                .ok_or_else(|| format!("anchor {name:?} is unknown for this build"))?;
            Ok(ResolvedTarget {
                rva: a.rva as usize,
                expect: a.expect_bytes().unwrap_or_default(),
            })
        }
        TargetRef::Raw { rva, expect } => Ok(ResolvedTarget {
            rva: *rva as usize,
            expect: hex::parse(expect).unwrap_or_default(),
        }),
    }
}

/// Read `len` bytes at `base+rva` — refuses (never crashes) on unmapped
/// memory; raw-RVA typo protection.
unsafe fn read_at(base: *const u8, rva: usize, len: usize) -> Result<Vec<u8>, String> {
    let p = unsafe { base.add(rva) } as *const c_void;
    if !unsafe { sys::is_readable(p, len) } {
        return Err(format!("rva {rva:#x} ({len} B) is not readable memory — refusing"));
    }
    Ok(unsafe { std::slice::from_raw_parts(p as *const u8, len) }.to_vec())
}

/// Read UTF-16 units at `base+rva` up to (excluding) the first NUL, bounded
/// by `limit` units — the C's bounded scan semantics. Also used by the
/// engine's banner cross-check.
pub unsafe fn read_u16s_bounded(
    base: *const u8,
    rva: usize,
    limit: usize,
) -> Result<Vec<u16>, String> {
    let raw = unsafe { read_at(base, rva, limit * 2)? };
    let mut units = Vec::with_capacity(256);
    for c in raw.chunks_exact(2) {
        let u = u16::from_le_bytes([c[0], c[1]]);
        if u == 0 {
            break;
        }
        units.push(u);
    }
    Ok(units)
}

/// Verify recorded original bytes at `base+rva+offset`.
unsafe fn verify(
    base: *const u8,
    rva: usize,
    offset: usize,
    expect: &[u8],
    what: &str,
) -> Result<(), String> {
    if expect.is_empty() {
        return Ok(()); // nothing recorded to verify
    }
    let cur = unsafe { read_at(base, rva + offset, expect.len())? };
    if cur.as_slice() != expect {
        return Err(format!(
            "{what}: memory differs from recorded bytes (found {}, expected {}) — refusing",
            hex::fmt(&cur),
            hex::fmt(expect)
        ));
    }
    Ok(())
}

// ------------------------------------------------------------------ [[patch]]

pub unsafe fn apply_patch(
    p: &Patch,
    entry: &BuildEntry,
    base: *const u8,
) -> Result<String, String> {
    let t = resolve_target(&p.target, entry)?;
    let data = hex::parse(&p.data).ok_or_else(|| "`data` is not valid hex".to_string())?;
    unsafe {
        verify(base, t.rva, 0, &t.expect, &format!("patch {} anchor bytes", describe(&p.target)))?;
        if let Some(e) = &p.expect {
            let e = hex::parse(e).unwrap_or_default();
            verify(base, t.rva, p.offset as usize, &e, "patch expect")?;
        }
    }
    let addr = unsafe { base.add(t.rva + p.offset as usize) } as *mut u8;
    if !unsafe { patch::write_bytes(addr, &data, sys::PAGE_READWRITE) } {
        return Err("VirtualProtect failed — write refused".into());
    }
    Ok(format!(
        "patch {} rva={:#x} off={} len={} expect=verified write=ok",
        describe(&p.target),
        t.rva,
        p.offset,
        data.len()
    ))
}

// ------------------------------------------------------------------ [[hook]]

pub unsafe fn apply_hook(
    h: &Hook,
    entry: &BuildEntry,
    base: *const u8,
    mod_dir: &Path,
) -> Result<String, String> {
    let t = resolve_target(&h.target, entry)?;
    match h.handler.as_str() {
        "train_list" => unsafe { apply_train_list(h, entry, base, mod_dir, &t) },
        "nop" => unsafe {
            verify(base, t.rva, 0, &t.expect, "hook nop anchor bytes")?;
            let addr = base.add(t.rva) as *mut u8;
            if !patch::write_bytes(addr, &patch::NOP_STUB, sys::PAGE_EXECUTE_READWRITE) {
                return Err("VirtualProtect failed — write refused".into());
            }
            Ok(format!(
                "hook nop@{} rva={:#x} stub installed (all valid forms kept)",
                describe(&h.target),
                t.rva
            ))
        },
        other => Err(format!("unknown handler {other:?}")),
    }
}

unsafe fn apply_train_list(
    h: &Hook,
    entry: &BuildEntry,
    base: *const u8,
    mod_dir: &Path,
    t: &ResolvedTarget,
) -> Result<String, String> {
    let mode = h.mode.unwrap_or(ListMode::Allow);
    let list_name = h.list.as_deref().unwrap_or_default();
    let text = std::fs::read_to_string(mod_dir.join(list_name))
        .map_err(|e| format!("list file {list_name:?}: {e}"))?;
    let allowed = forms::parse_allow_list(&text);
    if allowed.is_empty() {
        return Err(format!(
            "list file {list_name:?} contains no names — refusing \
             (use handler \"nop\" to keep all forms)"
        ));
    }
    let form_anchor = entry
        .anchor("form_list")
        .ok_or_else(|| "this build has no form_list anchor".to_string())?;
    let units = unsafe { read_u16s_bounded(base, form_anchor.rva as usize, FORMS_SCAN_LIMIT)? };
    let forms_list = forms::parse_form_list(&units);
    let report = forms::build_bitmap(&forms_list, &allowed);
    // allow: keep listed. deny: keep everything except the listed forms.
    let bitmap = match mode {
        ListMode::Allow => report.bitmap,
        ListMode::Deny => {
            let mut inv = report.bitmap.clone();
            for i in 0..forms_list.len().min(FORMS_MAX) {
                inv[i >> 3] ^= 1u8 << (i & 7);
            }
            inv
        }
    };
    let _ = G_ALLOWED.set(bitmap);
    // the per-call trace logger — the mods dir is the mod dir's parent
    if let Some(mods_dir) = mod_dir.parent() {
        let _ = G_TRACE.set(log::Logger::new(&mods_dir.join("a9loader.log")));
    }
    let f: FilterFn = my_filter;
    unsafe {
        verify(
            base,
            t.rva,
            0,
            &t.expect,
            &format!("hook train_list@{} anchor bytes", describe(&h.target)),
        )?;
        let addr = base.add(t.rva) as *mut u8;
        if !patch::write_bytes(addr, &patch::jmp_bytes(f as *const c_void), sys::PAGE_EXECUTE_READWRITE) {
            return Err("VirtualProtect failed — write refused".into());
        }
    }
    let mode_name = match mode {
        ListMode::Allow => "allow",
        ListMode::Deny => "deny",
    };
    Ok(format!(
        "hook train_list@{} rva={:#x} mode={} list={} ({} names: {} matched, {} unmatched; {} forms in exe) installed",
        describe(&h.target),
        t.rva,
        mode_name,
        list_name,
        allowed.len(),
        report.matched,
        report.unmatched,
        forms_list.len()
    ))
}
