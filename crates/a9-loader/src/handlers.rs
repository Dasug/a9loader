//! Effect handlers — the only code that runs in the game's own call path
//! (`my_filter`), plus the boot-time effect executors. Handlers are total:
//! no panics, no unwinding; every failure is a `Err(String)` the engine logs
//! while the game continues untouched.
//!
//! The original filter machinery lives on here as the `train_list` handler; the boot engine
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

/// The C scans at most 16384 UTF-16 units of the FORM list (as before).
const FORMS_SCAN_LIMIT: usize = 16384;
const FORMS_MAX: usize = forms::FORMS_MAX;

static G_ALLOWED: OnceLock<Box<[u8; FORMS_MAX / 8]>> = OnceLock::new();

/// per-call trace logger for the installed filter (the list windows call it
/// on every rebuild — the trace shows the INPUT list each time, which is
/// the observable of "trains disappear after cycling the type filter":
/// shrinking inputs = rows being invalidated between rebuilds)
static G_TRACE: OnceLock<log::Logger> = OnceLock::new();

/// The composed keep-bitmap the `train_list` filter serves — `None`
/// when no mod declared a `train_list` contribution (the filter hook was never
/// installed and the game's own 97-entry whitelist filter is in force). The harness
/// type-tab rebuild fix (`crate::tabrebuild`) re-derives tab membership from
/// exactly this set, so the purchase list survives a filter cycle intact.
pub fn composed_allowed() -> Option<&'static [u8; FORMS_MAX / 8]> {
    G_ALLOWED.get().map(|b| &**b)
}

/// Whether the installed composed keep-set admits `id` — `None` when no filter
/// was installed. The harness reads this to assert that a reserved train
/// id is admitted (the `[[train]]` ids are added to the bitmap programmatically).
pub fn composed_kept(id: usize) -> Option<bool> {
    composed_allowed().map(|bm| forms::kept(bm, id))
}

/// The harness seam: `a9loader_m7_kept(id)` -> 1 (kept), 2 (not kept) or
/// 0 (no composed filter installed).
#[unsafe(no_mangle)]
pub unsafe extern "system" fn a9loader_m7_kept(id: u64) -> u64 {
    match composed_kept(id as usize) {
        Some(true) => 1,
        Some(false) => 2,
        None => 0,
    }
}

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

/// The shipped purchase whitelist's fixed length (the exe table bound is
/// hard-coded to 97 entries; `train_list_whitelist` anchor + §9).
const WHITELIST_ENTRIES: usize = 97;

pub unsafe fn apply_hook(
    h: &Hook,
    entry: &BuildEntry,
    base: *const u8,
    mod_dir: &Path,
) -> Result<String, String> {
    let t = resolve_target(&h.target, entry)?;
    match h.handler.as_str() {
        // `train_list` contributions are composed across mods by the engine
        // and installed ONCE — this per-hook path only serves `nop`.
        "train_list" => {
            let _ = mod_dir;
            Err("train_list hooks are composed across mods by the engine \
                 (install_train_list) — this path is nop-only"
                .into())
        }
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

/// One mod's `train_list` contribution: the listed forms as an id bitmap, the
/// mode, and the resolved hook target. Parsed at boot with no globals touched —
/// the engine composes every contribution into ONE bitmap and installs ONE
/// hook (additive composition instead of a single owner).
pub struct Contribution {
    pub id: String,
    pub mode: ListMode,
    pub list_name: String,
    pub matched: usize,
    pub unmatched: usize,
    /// resolved target RVA (every contribution must name the same seam)
    pub rva: usize,
    pub expect: Vec<u8>,
    pub bitmap: Box<[u8; FORMS_MAX / 8]>,
}

/// Read + parse one `train_list` hook's mod-relative list file into a bitmap —
/// the same refusals as the pre-W3 single-mod path (missing/empty list, no
/// FORM anchor, unreadable FORM list).
pub unsafe fn train_list_contribution(
    h: &Hook,
    entry: &BuildEntry,
    base: *const u8,
    mod_dir: &Path,
    mod_id: &str,
) -> Result<Contribution, String> {
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
    let t = resolve_target(&h.target, entry)?;
    Ok(Contribution {
        id: mod_id.to_string(),
        mode,
        list_name: list_name.to_string(),
        matched: report.matched,
        unmatched: report.unmatched,
        rva: t.rva,
        expect: t.expect,
        bitmap: report.bitmap,
    })
}

/// One mod's `train_list` contribution carrying explicit reserved **ids**
/// : the `[[train]]` compiler allocates spec-row ids and the loader must
/// add those ids to the composed keep-bitmap PROGRAMMATICALLY — the name match
/// in [`train_list_contribution`] can never admit an injected row, because its
/// allocated name is deliberately not in the FORM list.
///
/// The contribution is an `Add` (the shipped whitelist stays the base) and
/// names the same `train_list_filter` seam, so it composes with every other
/// contribution unchanged.
pub unsafe fn reserved_id_contribution(
    mod_id: &str,
    ids: &[usize],
    entry: &BuildEntry,
) -> Result<Contribution, String> {
    let mut bitmap = Box::new([0u8; FORMS_MAX / 8]);
    let mut set = 0usize;
    for &id in ids {
        if id < FORMS_MAX {
            bitmap[id >> 3] |= 1u8 << (id & 7);
            set += 1;
        }
    }
    if set == 0 {
        return Err("no reserved ids to admit".into());
    }
    let t = resolve_target(
        &TargetRef::Anchor("train_list_filter".to_string()),
        entry,
    )?;
    Ok(Contribution {
        id: mod_id.to_string(),
        mode: ListMode::Add,
        list_name: format!("[[train]] reserved ids ({set})"),
        matched: set,
        unmatched: 0,
        rva: t.rva,
        expect: t.expect,
        bitmap,
    })
}

/// The shipped whitelist as a keep-bitmap — the baseline an `Add` contribution
/// builds on. Expect-verified and readability-guarded: a failure refuses the
/// `Add` loudly rather than silently offering every form.
pub unsafe fn read_vanilla_whitelist(
    entry: &BuildEntry,
    base: *const u8,
) -> Result<Box<[u8; FORMS_MAX / 8]>, String> {
    let anchor = entry
        .anchor("train_list_whitelist")
        .ok_or_else(|| "this build has no train_list_whitelist anchor".to_string())?;
    unsafe {
        verify(
            base,
            anchor.rva as usize,
            0,
            &anchor.expect_bytes().unwrap_or_default(),
            "train_list_whitelist anchor bytes",
        )?;
        let raw = read_at(base, anchor.rva as usize, WHITELIST_ENTRIES * 4)?;
        let mut bm = Box::new([0u8; FORMS_MAX / 8]);
        for c in raw.chunks_exact(4) {
            let id = u32::from_le_bytes([c[0], c[1], c[2], c[3]]) as usize;
            if id < FORMS_MAX {
                bm[id >> 3] |= 1u8 << (id & 7);
            }
        }
        Ok(bm)
    }
}

fn mode_name(m: ListMode) -> &'static str {
    match m {
        ListMode::Allow => "allow",
        ListMode::Add => "add",
        ListMode::Deny => "deny",
    }
}

/// Compose every `train_list` contribution into ONE keep-bitmap and install the
/// ONE filter hook. All contributions must resolve to the same target
/// RVA. Returns the journal line; a per-contribution breakdown goes to the log.
pub unsafe fn install_train_list(
    contribs: &[Contribution],
    entry: &BuildEntry,
    base: *const u8,
    forms_len: usize,
    logger: &log::Logger,
) -> Result<String, String> {
    let rva0 = contribs[0].rva;
    if let Some(bad) = contribs.iter().find(|c| c.rva != rva0) {
        return Err(format!(
            "train_list contributions resolve to different targets \
             ({rva0:#x} vs {:#x}) — refusing",
            bad.rva
        ));
    }
    let has_add = contribs.iter().any(|c| c.mode == ListMode::Add);
    let vanilla = if has_add {
        unsafe { read_vanilla_whitelist(entry, base)? }
    } else {
        Box::new([0u8; FORMS_MAX / 8])
    };
    let vanilla_n = (0..FORMS_MAX).filter(|&i| forms::kept(&vanilla, i)).count();
    let refs: Vec<(ListMode, &[u8; FORMS_MAX / 8])> =
        contribs.iter().map(|c| (c.mode, &*c.bitmap)).collect();
    let bitmap = forms::compose_filter(forms_len, &vanilla, &refs);
    let kept = (0..forms_len.min(FORMS_MAX))
        .filter(|&i| forms::kept(&bitmap, i))
        .count();
    let _ = G_ALLOWED.set(bitmap);
    let _ = G_TRACE.set(logger.clone());
    let f: FilterFn = my_filter;
    unsafe {
        verify(base, rva0, 0, &contribs[0].expect, "hook train_list anchor bytes")?;
        let addr = base.add(rva0) as *mut u8;
        if !patch::write_bytes(
            addr,
            &patch::jmp_bytes(f as *const c_void),
            sys::PAGE_EXECUTE_READWRITE,
        ) {
            return Err("VirtualProtect failed — write refused".into());
        }
    }
    let detail: Vec<String> = contribs
        .iter()
        .map(|c| {
            format!(
                "{}:{}({} matched, {} unmatched)",
                c.id,
                mode_name(c.mode),
                c.matched,
                c.unmatched
            )
        })
        .collect();
    for c in contribs {
        logger.line(&format!(
            "train_list contribution '{}': mode={} list={} ({} names: {} matched, {} unmatched)",
            c.id,
            mode_name(c.mode),
            c.list_name,
            c.matched + c.unmatched,
            c.matched,
            c.unmatched
        ));
    }
    Ok(format!(
        "hook train_list@train_list_filter rva={rva0:#x} composed {} contribution(s) [{}]{} \
         -> kept {kept} of {forms_len} forms installed",
        contribs.len(),
        detail.join(", "),
        if has_add {
            format!(" (add baseline = the shipped whitelist: {vanilla_n} forms)")
        } else {
            String::new()
        },
    ))
}
