//! DllMain glue — the M0 mode dispatch, a 1:1 behavioral port of
//! `apply_patch()` / `load_allowed()` in the C reference, plus (additive) log
//! lines. The C's silent-failure behavior is kept, just made visible.

use crate::sys::*;
use crate::{forms, ini, log, patch};
use core::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// RVAs (Steam build `5.00.5634`) — M2 moves these into the build address
/// book (`builds.toml` anchors).
pub const RVA_FILTER: usize = 0x1A13B0; // FUN_1401a13b0: train-list filter
pub const RVA_TABLE: usize = 0x6FC1C0; // 97-entry u32 whitelist
pub const RVA_FORMS: usize = 0x6FC430; // UTF-16 comma-separated FORM names

/// The C scans at most 16384 UTF-16 units of the FORM list.
const FORMS_SCAN_LIMIT: usize = 16384;
const FORMS_MAX: usize = forms::FORMS_MAX;

static G_ALLOWED: OnceLock<Box<[u8; FORMS_MAX / 8]>> = OnceLock::new();

type FilterFn =
    unsafe extern "system" fn(*mut c_void, *mut u32, *mut u32, *mut c_void) -> *mut c_void;

/// The filter replacement — identical to the C `my_filter`: keep only ids with
/// a bitmap bit, write the new end through `*out`, return `out`. Total: no
/// panics, no allocation, raw pointers only.
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
        while p != end {
            let id = *p;
            if let Some(a) = allowed {
                let i = id as usize;
                if i < FORMS_MAX && a[i >> 3] & (1u8 << (i & 7)) != 0 {
                    *w = id;
                    w = w.add(1);
                }
            }
            p = p.add(1);
        }
        *(out as *mut *mut u32) = w;
        out
    }
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

pub unsafe fn dllmain(hinst: HMODULE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason != DLL_PROCESS_ATTACH {
        return TRUE;
    }
    unsafe { DisableThreadLibraryCalls(hinst) };
    // Everything below is loader-lock-safe: file IO only (the M0 spike proved
    // std::fs at PROCESS_ATTACH), no LoadLibrary.
    log::line("---- a9loader M0 run ----");
    let Some(dir) = exe_dir() else {
        log::line("ERR: cannot resolve exe dir; no patch applied");
        return TRUE;
    };
    let mode = ini::read_mode(&dir);
    log::line(&format!("atrain9_proxy.ini: mode = {mode}"));
    unsafe { apply(&dir, &mode) };
    TRUE
}

unsafe fn apply(dir: &Path, mode: &str) {
    unsafe {
        let base = GetModuleHandleW(std::ptr::null()) as *mut u8;
        if base.is_null() {
            log::line("ERR: GetModuleHandle(NULL) failed; no patch applied");
            return;
        }
        match mode {
            "none" => log::line("mode=none: no patch (pure proxy)"),

            "nop" => {
                let ok = patch::write_bytes(
                    base.add(RVA_FILTER),
                    &patch::NOP_STUB,
                    PAGE_EXECUTE_READWRITE,
                );
                log::line(if ok {
                    "mode=nop: filter stub installed (all valid forms kept)"
                } else {
                    "ERR: nop patch: VirtualProtect failed"
                });
            }

            "table113" => {
                let cell = base.add(RVA_TABLE + 6 * 4);
                let ok = patch::write_bytes(cell, &136u32.to_ne_bytes(), PAGE_READWRITE);
                let v = *(cell as *const u32);
                log::line(&format!(
                    "mode=table113: tbl[6] = {v} {}",
                    if ok { "(ok)" } else { "(VirtualProtect FAILED)" }
                ));
            }

            other => {
                let fname = match other {
                    "model" => "atrain9_filter_model.txt",
                    "modeltex" => "atrain9_filter_modeltex.txt",
                    _ => "atrain9_filter.txt",
                };
                if !matches!(other, "filter" | "model" | "modeltex") {
                    log::line(&format!(
                        "WARN: unknown mode {other:?} — treated as filter (C parity)"
                    ));
                }
                install_filter(dir, base, other, fname);
            }
        }
    }
}

unsafe fn install_filter(dir: &Path, base: *mut u8, mode: &str, fname: &str) {
    unsafe {
        // FORM list from the mapped exe image (bounded scan, like the C).
        let list = base.add(RVA_FORMS) as *const u16;
        let mut units = Vec::with_capacity(2048);
        for i in 0..FORMS_SCAN_LIMIT {
            let c = *list.add(i);
            units.push(c);
            if c == 0 {
                break;
            }
        }
        let forms_list = forms::parse_form_list(&units);

        let allowed = match std::fs::read_to_string(dir.join(fname)) {
            Ok(t) if !t.is_empty() => forms::parse_allow_list(&t),
            Ok(_) => {
                log::line(&format!(
                    "WARN: {fname} is empty — 0 forms allowed (hook still installed, C parity)"
                ));
                Vec::new()
            }
            Err(_) => {
                log::line(&format!(
                    "WARN: {fname} missing/unreadable — 0 forms allowed (hook still installed, C parity)"
                ));
                Vec::new()
            }
        };
        let report = forms::build_bitmap(&forms_list, &allowed);
        let _ = G_ALLOWED.set(report.bitmap);
        let f: FilterFn = my_filter;
        let ok = patch::write_bytes(
            base.add(RVA_FILTER),
            &patch::jmp_bytes(f as *const c_void),
            PAGE_EXECUTE_READWRITE,
        );
        log::line(&format!(
            "mode={mode}: filter hook {} — {}/{} names matched, {} unmatched, {} forms in exe list",
            if ok { "installed" } else { "FAILED (VirtualProtect)" },
            report.matched,
            allowed.len(),
            report.unmatched,
            forms_list.len(),
        ));
    }
}
