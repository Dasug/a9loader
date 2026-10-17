//! Loader diagnostics shared by both injection paths (opt-in): the panic-hook
//! replacement (a plain message via OutputDebugStringA instead of a symbolizer
//! that dies under Wine), the consist-row copier / object-creator probes, and
//! the per-`train_list_builder` spec-row dump.

use super::spec::{
    MANAGER_RVA, MGR_ROWS, ROW_NAME_BASE, ROW_NAMEALT_BASE, ROW_PFORMNAME, ROW_STRIDE,
};
use super::train::TrainState;
use crate::{log, patch, sys};
use core::ffi::c_void;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// DIAGNOSTIC: capture a panic message without allocation or a backtrace.
//
// The default panic path prints a backtrace, and under Wine its symbolizer
// (gimli/addr2line) is what dies — that is why a loader panic looked like a
// mystery crash. Replacing the hook costs nothing and makes the next panic
// readable. Installed once when trains are injected.
// ---------------------------------------------------------------------------
static PANIC_MSG: Mutex<Option<String>> = Mutex::new(None);

unsafe extern "system" {
    fn OutputDebugStringA(lpOutputString: *const u8);
}

fn capture_panic(info: &std::panic::PanicHookInfo<'_>) {
    {
        static mut BUF: [u8; 512] = [0; 512];
        unsafe {
            let buf = &mut *core::ptr::addr_of_mut!(BUF);
            let prefix = b"a9loader PANIC: <no payload>\0";
            let mut n = 0usize;
            while n < prefix.len() && n + 1 < buf.len() {
                buf[n] = prefix[n];
                n += 1;
            }
            buf[n] = 0;
            OutputDebugStringA(buf.as_ptr());
        }
    }
    let text = if let Some(t) = info.payload().downcast_ref::<&str>() {
        (*t).to_string()
    } else if let Some(t) = info.payload().downcast_ref::<String>() {
        t.clone()
    } else {
        "<non-string panic payload>".to_string()
    };
    if let Ok(mut slot) = PANIC_MSG.try_lock() {
        *slot = Some(match info.location() {
            Some(l) => format!("{text} (at {l})"),
            None => text,
        });
    }
}

/// Replace the default panic hook (whose backtrace symbolizer is what dies under
/// Wine) so a loader panic is at least named. DIAGNOSTIC ONLY.
pub fn install_panic_capture(_logger: &log::Logger) {
    let _ = std::panic::take_hook();
    std::panic::set_hook(Box::new(|info| capture_panic(info)));
}
// ---------------------------------------------------------------------------
// DIAGNOSTIC: the consist row copier (address in `builds.toml`).
//
// The copier call (object, src, n) memcpy's a row-shaped struct into a consist
// object's +0x4e0 window. The per-car entries `train_row_car_wiring` resolves
// through land at obj+0x50c = 0x4e0 + 0x2c, so this hook shows the {form, aux}
// pairs the renderer actually uses — for vanilla trains and ours alike, which is
// the comparison that has been missing all along. Diagnostic only: the original
// runs first and its result is what the caller sees.
// ---------------------------------------------------------------------------
pub(super) static CONSIST_TRAMP: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
static CONSIST_LOG: std::sync::OnceLock<log::Logger> = std::sync::OnceLock::new();
static CONSIST_CALLS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

type ConsistCopyFn = unsafe extern "system" fn(*mut c_void, *const c_void, u32);

unsafe extern "system" fn my_consist_copy(obj: *mut c_void, src: *const c_void, n: u32) {
    unsafe {
        let tramp = CONSIST_TRAMP.load(core::sync::atomic::Ordering::Relaxed);
        if tramp == 0 {
            return;
        }
        // log on ENTRY: if the function runs at all this line appears, whatever the
        // dumps below decide to do (that is how we tell "never called" from "called
        // but filtered out")
        if let Some(l) = CONSIST_LOG.get() {
            let c = CONSIST_CALLS.load(core::sync::atomic::Ordering::Relaxed);
            if c < 5 {
                l.line(&format!(
                    "probe: consist build ENTER #{c} obj={obj:p} src={src:p} n={n}"
                ));
            }
        }
        let f: ConsistCopyFn = core::mem::transmute(tramp);
        f(obj, src, n);
        let Some(logger) = CONSIST_LOG.get() else {
            return;
        };
        let call = CONSIST_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if call >= 60 {
            return;
        }
        let ob = obj as *const u8;
        if !sys::is_readable(ob as *const c_void, 0x6b0) {
            return;
        }
        let count = *ob.add(0xbc);
        let mut ents = String::new();
        for k in 0..10usize {
            let e = ob.add(0x50c + k * 0x18);
            ents.push_str(&format!(
                " [{k}]={{{} , {}, {}}}",
                *(e as *const u16),
                *e.add(2),
                *e.add(3)
            ));
        }
        let mut srcents = String::new();
        if !src.is_null() && sys::is_readable(src, 0x100) {
            let sb = src as *const u8;
            for k in 0..10usize {
                let e = sb.add(0x2c + k * 0x18);
                srcents.push_str(&format!(
                    " [{k}]={{{} , {}, {}}}",
                    *(e as *const u16),
                    *e.add(2),
                    *e.add(3)
                ));
            }
        }
        // the SOURCE row's scalar fields, sampled at CONSIST-BUILD time (late, with the
        // window open) — the row probe only ever sees the boot-time values, and this is
        // what tells us whether a field like +0x121 is a constant or a live value.
        let mut src_scal = String::new();
        if !src.is_null() && sys::is_readable(src, 0x140) {
            let sb = src as *const u8;
            let sc = |o: usize| -> u16 { *(sb.add(o) as *const u16) };
            src_scal = format!(
                " src scalars: +14={} +16={} +18={} +1a={} +1c={} +1e={} +38={} +9d={} +121={} +124={}",
                sc(0x14),
                sc(0x16),
                sc(0x18),
                sc(0x1a),
                sc(0x1c),
                sc(0x1e),
                *sb.add(0x38),
                *sb.add(0x9d),
                *sb.add(0x121),
                *sb.add(0x124),
            );
        }
        logger.line(&format!(
            "probe: consist copy #{call} obj={obj:p} src={src:p} n={n} carCount={count}{src_scal}\n\
             probe:   copied{ents}\n\
             probe:   source{srcents}"
        ));
    }
}

/// Install the consist-copy probe (diagnostic; fail-open on refusal).
pub(super) unsafe fn install_consist_probe(
    logger: &log::Logger,
    base: *const u8,
    rva: usize,
    expect: &[u8],
) -> bool {
    unsafe {
        let entry = base.cast_mut().add(rva);
        match patch::install_trampoline(
            entry,
            my_consist_copy as *const c_void,
            expect,
            sys::PAGE_EXECUTE_READWRITE,
        ) {
            Ok(t) => {
                CONSIST_TRAMP.store(t.as_ptr() as usize, core::sync::atomic::Ordering::Relaxed);
                let _ = CONSIST_LOG.set(logger.clone());
                logger.line(&format!(
                    "probe: consist_row_copy hook installed rva={rva:#x} expect=verified"
                ));
                true
            }
            Err(e) => {
                logger.warn(&format!(
                    "probe refused: consist_row_copy rva={rva:#x}: {e} — the consist entries \
                     stay unmeasured (diagnostic only)"
                ));
                false
            }
        }
    }
}

// ---------------------------------------------------------------------------
// DIAGNOSTIC: the consist object CREATOR (one level above the filler).
//
// The creator call (manager, index, ?, src, n) allocates the 0x6b8-byte consist
// object, parks it at [manager]+index*8 and then runs the filler. Hooking it
// too separates "the purchase path never ran" from "the filler was bypassed".
// ---------------------------------------------------------------------------
pub(super) static CREATE_TRAMP: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
static CREATE_CALLS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

type ConsistCreateFn =
    unsafe extern "system" fn(*mut c_void, i32, u64, *const c_void, u32) -> *mut c_void;

unsafe extern "system" fn my_consist_create(
    manager: *mut c_void,
    index: i32,
    a3: u64,
    src: *const c_void,
    n: u32,
) -> *mut c_void {
    unsafe {
        let tramp = CREATE_TRAMP.load(core::sync::atomic::Ordering::Relaxed);
        if tramp == 0 {
            return core::ptr::null_mut();
        }
        if let Some(l) = CONSIST_LOG.get() {
            let c = CREATE_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if c < 8 {
                let form = if src.is_null() || !sys::is_readable(src, 0x40) {
                    0
                } else {
                    *(src as *const u16).add(0x2c / 2)
                };
                l.line(&format!(
                    "probe: consist create ENTER #{c} mgr={manager:p} index={index} src={src:p} \
                     n={n} src.form={form}"
                ));
            }
        }
        let f: ConsistCreateFn = core::mem::transmute(tramp);
        f(manager, index, a3, src, n)
    }
}

/// Install the consist-creator probe (diagnostic; fail-open on refusal).
pub(super) unsafe fn install_consist_create(
    logger: &log::Logger,
    base: *const u8,
    rva: usize,
    expect: &[u8],
) -> bool {
    unsafe {
        let entry = base.cast_mut().add(rva);
        match patch::install_trampoline(
            entry,
            my_consist_create as *const c_void,
            expect,
            sys::PAGE_EXECUTE_READWRITE,
        ) {
            Ok(t) => {
                CREATE_TRAMP.store(t.as_ptr() as usize, core::sync::atomic::Ordering::Relaxed);
                let _ = CONSIST_LOG.set(logger.clone());
                logger.line(&format!(
                    "probe: consist_object_create hook installed rva={rva:#x} expect=verified"
                ));
                true
            }
            Err(e) => {
                logger.warn(&format!(
                    "probe refused: consist_object_create rva={rva:#x}: {e} — the purchase \
                 path stays unmeasured (diagnostic only)"
                ));
                false
            }
        }
    }
}
/// DIAGNOSTIC: dump what the loader can see of the spec table at each
/// `train_list_builder` call.
///
/// The live acceptance boot refused twice with "donor row 195 (FORM_KH32) is not
/// populated yet (bValid == 0)", including at a call that came AFTER the boot had
/// finished — so either the row index we look up is not the row the game fills,
/// or the fill happens somewhere we never observe. This prints the vector's real
/// extent, the validity byte at a few known indices, the first populated index,
/// the name strings at 195 and 212, and where (if anywhere) a row actually calls
/// itself FORM_KH32. One boot with this in the log settles it.
pub(super) unsafe fn probe_row_map(g: &TrainState, call: u32, mgr: *mut u8) {
    unsafe {
        let ours = g.base + MANAGER_RVA;
        let vec = (ours + MGR_ROWS) as *const usize;
        let (begin, end) = if sys::is_readable(vec as *const c_void, 0x18) {
            (*vec, *vec.add(1))
        } else {
            (0, 0)
        };
        let count = if begin != 0 && end >= begin {
            (end - begin) / ROW_STRIDE
        } else {
            0
        };

        let name_at = |idx: usize| -> String {
            if idx >= count {
                return "<oob>".into();
            }
            let row = (begin + idx * ROW_STRIDE) as *const u8;
            if !sys::is_readable(row as *const c_void, ROW_STRIDE) {
                return "<unreadable>".into();
            }
            let p = *(row.add(ROW_PFORMNAME) as *const *const u16);
            if p.is_null() || !sys::is_readable(p as *const c_void, 2) {
                return "<null>".into();
            }
            let mut s = String::new();
            for i in 0..24 {
                let c = *p.add(i);
                if c == 0 {
                    break;
                }
                s.push(char::from_u32(c as u32).unwrap_or('?'));
            }
            s
        };
        // ---------------------------------------------------------------------
        // (the model-registry and consist-source walks that used to live here were
        // removed 2026-10-17: they were the most verbose thing in the loader and had
        // answered their question. The car build logs every model index it takes.)

        // DIAGNOSTIC (2026-10-16): where the coupling block really starts.
        //
        // AR4's live row has its 4-car coupling `01 02 02 ff` at +0x55, its 5-car
        // `01 02 02 02 ff` at +0x5f — a 10-byte stride. With 10 records (one per
        // consist length) that puts the block base at +0x37, NOT the +0x39 this
        // loader writes. This dumps the whole region for a few shipped trains whose
        // train_pattern.csv groups are known, so the base and the stride fall out of
        // several data points instead of one.
        // 417 is the game's own overflow row: roster_row_builder clamps every car
        // entry of an injected train to it, so ITS models are what such a train renders.
        for idx in [3usize, 96, 159, 195, 212, 417, 421, 422] {
            if idx >= count {
                continue;
            }
            let row = (begin + idx * ROW_STRIDE) as *const u8;
            if !sys::is_readable(row as *const c_void, ROW_STRIDE) {
                continue;
            }
            let mut u16s = String::new();
            for k in 0..6 {
                u16s.push_str(&format!(" {}", *(row.add(0x2c + k * 2) as *const u16)));
            }
            // the scalar fields the consist/panel/viewer read, so a mod row can be
            // compared field-by-field against a shipped one
            let scal = |o: usize| -> u16 { *(row.add(o) as *const u16) };
            g.logger.debug(&format!(
                "probe: row {idx} ({}) scalars: +14={} +16={} +18={} +1a={} +1c={} +1e={} \
                 +38={} +9d={} +121={} +124={}",
                name_at(idx),
                scal(0x14),
                scal(0x16),
                scal(0x18),
                scal(0x1a),
                scal(0x1c),
                scal(0x1e),
                *row.add(0x38),
                *row.add(0x9d),
                *row.add(0x121),
                *row.add(0x124),
            ));
            g.logger.debug(&format!(
                "probe: row {idx} ({}) u16[6]@0x2c ={u16s}",
                name_at(idx)
            ));
            for base in [0x39usize] {
                let mut recs = String::new();
                for l in 0..10 {
                    let mut r = String::new();
                    for j in 0..10 {
                        r.push_str(&format!("{:02x}", *row.add(base + l * 10 + j)));
                    }
                    recs.push_str(&format!(" [{l}]={r}"));
                }
                g.logger
                    .debug(&format!("probe: row {idx} base {base:#04x}{recs}"));
            }
            let mut hex = String::new();
            for k in 0x2c..0x9c {
                hex.push_str(&format!("{:02x}", *row.add(k)));
            }
            g.logger
                .debug(&format!("probe: row {idx} bytes@0x2c..0x9c {hex}"));
        }

        // DIAGNOSTIC: the 18 name lanes of a row (9 at +0xa8, 9 at +0xf0). The
        // panel's price/capacity/description come from the CSV tables, whose keys
        // are UPPERCASE FORM names, and the injected row's pFormName is lowercase
        // — so whatever name the lookup uses is one of these lanes, and after the
        // clone the unpatched ones still hold the DONOR's names. Dumping them for
        // the donor and the injected row shows exactly which lane has to change.
        let lanes_at = |idx: usize| -> String {
            if idx >= count {
                return "<oob>".into();
            }
            let row = (begin + idx * ROW_STRIDE) as *const u8;
            if !sys::is_readable(row as *const c_void, ROW_STRIDE) {
                return "<unreadable>".into();
            }
            let mut out = Vec::new();
            for (base_off, tag) in [(ROW_NAME_BASE, "n"), (ROW_NAMEALT_BASE, "a")] {
                for k in 0..9 {
                    let off = base_off + k * 0x8;
                    if off + 8 > ROW_STRIDE {
                        break;
                    }
                    let p = *(row.add(off) as *const *const u16);
                    if p.is_null() || !sys::is_readable(p as *const c_void, 2) {
                        out.push(format!("{tag}{k}=<null>"));
                        continue;
                    }
                    let mut s = String::new();
                    for i in 0..20 {
                        let c = *p.add(i);
                        if c == 0 {
                            break;
                        }
                        s.push(char::from_u32(c as u32).unwrap_or('?'));
                    }
                    out.push(format!("{tag}{k}={s}"));
                }
            }
            out.join(" ")
        };
        let b0 = |idx: usize| -> String {
            if idx >= count {
                return "<oob>".into();
            }
            let row = (begin + idx * ROW_STRIDE) as *const u8;
            if !sys::is_readable(row as *const c_void, 1) {
                return "?".into();
            }
            (*row).to_string()
        };
        let first = (0..count)
            .find(|&i| b0(i) == "1")
            .map(|i| i.to_string())
            .unwrap_or_else(|| "none".into());
        let lower = |s: &str| s.to_ascii_lowercase();
        let populated: Vec<usize> = (0..count).filter(|&i| b0(i) == "1").collect();
        let sample: Vec<String> = populated
            .iter()
            .take(12)
            .map(|&i| format!("{i}:{}", name_at(i)))
            .collect();
        let cand = |want: &str| -> String {
            (0..count)
                .find(|&i| lower(&name_at(i)) == lower(want))
                .map(|i| {
                    if b0(i) == "1" {
                        format!("{i}:valid")
                    } else {
                        format!("{i}:INVALID")
                    }
                })
                .unwrap_or_else(|| "absent".into())
        };

        g.logger.debug(&format!(
            "train probe: call {call} mgr={mgr:p} ours={ours:#x} count={count} \
             populated={} b0[0]={} b0[62]={} b0[195]={} b0[212]={} first_valid={first} \
             name[195]={:?} name[212]={:?} FORM_KH32_at={} | AR4={} 285={} 201KY={} \
             sample=[{}]",
            populated.len(),
            b0(0),
            b0(62),
            b0(195),
            b0(212),
            name_at(195),
            name_at(212),
            (0..count)
                .find(|&i| lower(&name_at(i)) == "form_kh32")
                .map(|i| i.to_string())
                .unwrap_or_else(|| "none".into()),
            cand("FORM_AR4"),
            cand("FORM_285"),
            cand("FORM_201KY"),
            sample.join(", ")
        ));
        if count > 0 {
            g.logger
                .debug(&format!("train probe lanes: donor(96) [{}]", lanes_at(96)));
            if count > 421 {
                g.logger.debug(&format!(
                    "train probe lanes: injected(421) [{}]",
                    lanes_at(421)
                ));
            }
            // DIAGNOSTIC: the raw bytes of the donor row and of the
            // injected row. The panel's price/capacity/configuration are the train's
            // pattern-group aggregates (the group head for FORM_AR4 is at data row
            // 2322 with capacity 168 / price 528,000,000 — neither of which is what
            // the panel showed), so the row must still reference the DONOR's group
            // somewhere in these bytes. A field-by-field diff pins the offset.
            let hex_at = |idx: usize| -> String {
                if idx >= count {
                    return "<oob>".into();
                }
                let row = (begin + idx * ROW_STRIDE) as *const u8;
                if !sys::is_readable(row as *const c_void, ROW_STRIDE) {
                    return "<unreadable>".into();
                }
                let mut s = String::new();
                for i in 0..0x9e {
                    s.push_str(&format!("{:02x}", *row.add(i)));
                }
                s
            };
            g.logger
                .debug(&format!("train probe bytes: donor(96) {}", hex_at(96)));
            // (the reserved rows do not exist yet here — the raw byte dump happens
            // in the car build, which runs after the injection)
        }
    }
}
