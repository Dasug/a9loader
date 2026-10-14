//! DIAGNOSTIC ONLY — OFF BY DEFAULT, not part of normal loader operation.
//!
//! Nothing here runs unless `probe_window = true` is set in `a9loader.toml`.
//! It observes the corpus notes/23 purchase-window type-cycle quirk live.
//!
//! Two trampoline-only hooks, both dumping the spec-table manager's three index
//! vectors (`+0x30` unfiltered, `+0x48` the list the UI reads, `+0x60` the
//! type-group list):
//!
//! - the **type-tab rebuild** `FUN_14019e760` — the precise cycle engine the
//!   big list window calls with the selected tab's type list; its entry/exit
//!   bracket the rebuild, so the sizes before/after and the rows that vanished
//!   are exactly the quirk's observable. Its exit line also names what the
//!   vanilla 97-scan would have kept, next to what the (fixed) composed scan
//!   keeps — the mechanism and the fix in one line;
//! - the **big list window** `train_list_big_list_window` — the per-frame shell
//!   that calls it, so the frame in which the filter was cycled is bracketed.
//!
//! A dump is emitted only when a size CHANGES since the previous observation
//! (plus the first call); the window runs per frame while it is open, so a
//! per-call dump would be noise. Each dump logs the three sizes and, for
//! `+0x48`, per row the display-name pointer, the first 40 wide chars and the
//! row's type byte(s); a delta line names the rows that appeared/vanished.
//!
//! Fail-open: a missing anchor, a failed expect-verify or an unreadable manager
//! logs one loud line and leaves the boot vanilla. Neither hook panics or
//! changes behaviour.

use crate::{forms, handlers, log, patch, probe, sys};
use a9_mod::builds::BuildEntry;
use core::ffi::c_void;
use std::sync::Mutex;
use std::sync::OnceLock;

/// The type-tab rebuild entry (builds.toml anchor).
pub const ANCHOR_TAB: &str = "train_list_tab_rebuild";
/// The big list window entry (builds.toml anchor).
pub const ANCHOR_WINDOW: &str = "train_list_window_entry";
/// The spec-table manager singleton (data anchor, no expect bytes).
pub const ANCHOR_MANAGER: &str = "train_spec_manager";

/// The manager's fields (see notes/18 §2).
const MGR_ROWS: usize = 0x18;
const MGR_ROWS_END: usize = 0x20;
const VEC_UNFILTERED: usize = 0x30;
const VEC_FILTERED: usize = 0x48;
const VEC_TYPEGROUP: usize = 0x60;
const ROW_STRIDE: usize = 0x140;
const ROW_TYPE: usize = 0x04;
/// The row's JP/EN display names are raw CString DATA POINTERS at +0xa8/+0xb0
/// (`wstr_assign_thunk` stores `*(row+0xa8) = data_ptr`; the CString's
/// refcount header lives at `data_ptr-0x18`). The game picks the lane with the
/// manager's language offset (`manager+0x90`, 0 = JP / 8 = EN) — see
/// FUN_1401fe780 (`PTR_140e14528[id].aName + _DAT_140e145a0`).
const ROW_NAME_JP: usize = 0xa8;
/// The manager's display-name language lane offset (0 or 8).
const MGR_LANG_OFF: usize = 0x90;
/// The 0x2c-stride tab descriptor table (notes/23).
const TAB_TABLE_RVA: usize = 0x68e950;
const TAB_STRIDE: usize = 0x2c;
/// Wide chars logged per name.
const NAME_CHARS: usize = 40;
/// Per-row dumps are capped so the log stays readable; delta lines always run.
const FULL_DUMPS: usize = 3;
const MAX_ROWS_DUMPED: usize = 500;
const MAX_DELTA: usize = 40;

type TabFn = unsafe extern "system" fn(*mut u8, *const i32);
type WindowFn = unsafe extern "system" fn(*mut c_void);

struct Cursor {
    last: [usize; 3],
    seen: bool,
    calls: u64,
    full_left: usize,
    prev: Vec<u32>,
}

impl Cursor {
    fn new() -> Self {
        Self {
            last: [0; 3],
            seen: false,
            calls: 0,
            full_left: FULL_DUMPS,
            prev: Vec::new(),
        }
    }
}

struct Inner {
    logger: log::Logger,
    manager: usize,
    tab_table: usize,
    vanilla: Box<[u8; forms::FORMS_MAX / 8]>,
    tab_tramp: Option<patch::Trampoline>,
    win_tramp: Option<patch::Trampoline>,
    tab: Cursor,
    win: Cursor,
}

static G: OnceLock<Mutex<Inner>> = OnceLock::new();

/// Install the diagnostic hooks. Both anchors are expect-verified; a failure of
/// either is reported in the returned lines and the other still installs.
pub unsafe fn install(
    logger: log::Logger,
    base: *const u8,
    entry: &BuildEntry,
) -> Result<Vec<String>, String> {
    unsafe {
        let mgr = entry
            .anchor(ANCHOR_MANAGER)
            .ok_or_else(|| format!("this build has no {ANCHOR_MANAGER} anchor"))?;
        let vanilla = handlers::read_vanilla_whitelist(entry, base)
            .unwrap_or_else(|_| Box::new([0u8; forms::FORMS_MAX / 8]));
        let _ = G.set(Mutex::new(Inner {
            logger,
            manager: base as usize + mgr.rva as usize,
            tab_table: base as usize + TAB_TABLE_RVA,
            vanilla,
            tab_tramp: None,
            win_tramp: None,
            tab: Cursor::new(),
            win: Cursor::new(),
        }));
        let mut lines = Vec::new();
        let mut hook = |anchor: &str, hook_fn: *const c_void, what: &str, slot: Slot| {
            match entry.anchor(anchor) {
                Some(a) => match patch::install_trampoline(
                    base.cast_mut().add(a.rva as usize),
                    hook_fn,
                    &a.expect_bytes().unwrap_or_default(),
                    sys::PAGE_EXECUTE_READWRITE,
                ) {
                    Ok(t) => {
                        if let Ok(mut g) = G.get().unwrap().lock() {
                            match slot {
                                Slot::Tab => g.tab_tramp = Some(t),
                                Slot::Win => g.win_tramp = Some(t),
                            }
                        }
                        lines.push(format!(
                            "probe: {what} hook installed rva={:#x} expect=verified",
                            a.rva
                        ));
                    }
                    Err(e) => lines.push(format!(
                        "probe refused: {what} rva={:#x}: {e} — its dump is off",
                        a.rva
                    )),
                },
                None => lines.push(format!("probe refused: this build has no {anchor} anchor")),
            }
        };
        hook(
            ANCHOR_TAB,
            my_tab_rebuild as *const c_void,
            "tab rebuild (FUN_14019e760 — the type-cycle engine)",
            Slot::Tab,
        );
        hook(
            ANCHOR_WINDOW,
            my_window as *const c_void,
            "window (train_list_big_list_window — the frame bracket)",
            Slot::Win,
        );
        Ok(lines)
    }
}

enum Slot {
    Tab,
    Win,
}

/// The `u32` index-vector length at `manager+off` (begin/end at off/off+8).
unsafe fn vec_len(manager: usize, off: usize) -> Option<usize> {
    unsafe {
        let m = manager as *const u8;
        if !sys::is_readable(m.add(off).cast(), 16) {
            return None;
        }
        let begin = *(m.add(off) as *const *const u8);
        let end = *(m.add(off + 8) as *const *const u8);
        Some((end as usize).checked_sub(begin as usize)? / 4)
    }
}

/// The index vector at `manager+off`'s contents (capped).
unsafe fn vec_contents(manager: usize, off: usize, cap: usize) -> Vec<u32> {
    unsafe {
        let m = manager as *const u8;
        let begin = *(m.add(off) as *const *const u8);
        let end = *(m.add(off + 8) as *const *const u8);
        let Some(span) = (end as usize).checked_sub(begin as usize) else {
            return Vec::new();
        };
        let n = (span / 4).min(cap);
        if n == 0 || !sys::is_readable(begin.cast(), n * 4) {
            return Vec::new();
        }
        std::slice::from_raw_parts(begin as *const u32, n).to_vec()
    }
}

/// The three list sizes.
unsafe fn sizes(manager: usize) -> [usize; 3] {
    unsafe {
        [
            vec_len(manager, VEC_UNFILTERED).unwrap_or(0),
            vec_len(manager, VEC_FILTERED).unwrap_or(0),
            vec_len(manager, VEC_TYPEGROUP).unwrap_or(0),
        ]
    }
}

/// One row's display name pointer + first 40 wide chars + type bytes.
unsafe fn row_desc(manager: usize, id: u32) -> String {
    unsafe {
        let m = manager as *const u8;
        let rows = *(m.add(MGR_ROWS) as *const *const u8);
        let rows_end = *(m.add(MGR_ROWS_END) as *const *const u8);
        let nrows = (rows_end as usize - rows as usize) / ROW_STRIDE;
        if (id as usize) >= nrows {
            return format!("id={id} <out of range>");
        }
        let row = rows.add(id as usize * ROW_STRIDE);
        if !sys::is_readable(row.cast(), 0xc0) {
            return format!("id={id} <row unreadable>");
        }
        let ty = *(row.add(ROW_TYPE) as *const i32);
        // the game's lane: the JP (0) or EN (8) name pointer at row+0xa8+lane
        let lane = if sys::is_readable(m.add(MGR_LANG_OFF).cast(), 4) {
            (*(m.add(MGR_LANG_OFF) as *const u32) as usize).min(8) & !7
        } else {
            0
        };
        let name_slot = row.add(ROW_NAME_JP + lane) as *const *const u16;
        let name_ptr = name_slot as *const c_void;
        // read the CString data pointer directly (the fetch probe's `[r8]`
        // case): `wide_str_indirect`'s ASCII head-sanity rejects CJK names,
        // so it answers "<unreadable>" for the JP lane on every row.
        let data = std::ptr::read(name_slot);
        let name = if sys::is_readable(data.cast(), 2) {
            probe::wide_str(data)
        } else {
            None
        };
        let name = name.unwrap_or_else(|| "<unreadable>".into());
        let name: String = name.chars().take(NAME_CHARS).collect();
        format!("id={id} type={ty} name=@{:#x} {name:?}", name_ptr as usize)
    }
}

/// The full dump of the three lists for one observed call.
unsafe fn dump(logger: &log::Logger, manager: usize, cur: &Cursor, tag: &str, call: u64) {
    unsafe {
        let [a, b, c] = sizes(manager);
        logger.line(&format!(
            "probe: {tag} #{call} lists +0x30={a} +0x48={b} +0x60={c}"
        ));
        if cur.full_left == 0 {
            return;
        }
        let ids = vec_contents(manager, VEC_FILTERED, MAX_ROWS_DUMPED);
        for id in &ids {
            logger.line(&format!("probe: {tag}   +0x48 {}", row_desc(manager, *id)));
        }
        if b > MAX_ROWS_DUMPED {
            logger.line(&format!(
                "probe: {tag}   +0x48 … {} more rows (capped)",
                b - MAX_ROWS_DUMPED
            ));
        }
    }
}

/// The delta line: which ids left/joined `+0x48` since the previous dump.
unsafe fn delta(logger: &log::Logger, manager: usize, cur: &mut Cursor, tag: &str) {
    unsafe {
        let now = vec_contents(manager, VEC_FILTERED, MAX_ROWS_DUMPED);
        let removed: Vec<u32> = cur
            .prev
            .iter()
            .copied()
            .filter(|id| !now.contains(id))
            .collect();
        let added: Vec<u32> = now
            .iter()
            .copied()
            .filter(|id| !cur.prev.contains(id))
            .collect();
        if !removed.is_empty() || !added.is_empty() {
            let fmt = |ids: &[u32]| -> String {
                let shown: Vec<String> = ids
                    .iter()
                    .take(MAX_DELTA)
                    .map(|id| format!("({})", row_desc(manager, *id)))
                    .collect();
                format!(
                    "{}{}",
                    shown.join(" "),
                    if ids.len() > MAX_DELTA { " …" } else { "" }
                )
            };
            logger.line(&format!(
                "probe: {tag}   +0x48 delta: -{} rows {} +{} rows {}",
                removed.len(),
                fmt(&removed),
                added.len(),
                fmt(&added)
            ));
        }
        cur.prev = now;
    }
}

/// Count what the VANILLA 97-scan would have kept for this tab.
unsafe fn vanilla_would_keep(
    manager: usize,
    vanilla: &[u8; forms::FORMS_MAX / 8],
    tab: *const i32,
) -> usize {
    unsafe {
        let m = manager as *const u8;
        if !sys::is_readable(m.add(MGR_ROWS).cast(), 16) {
            return 0;
        }
        let rows = *(m.add(MGR_ROWS) as *const *const u8);
        let rows_end = *(m.add(MGR_ROWS_END) as *const *const u8);
        let nrows = (rows_end as usize - rows as usize) / ROW_STRIDE;
        let tab_ok = !tab.is_null() && sys::is_readable(tab.cast(), 40);
        let mut kept = 0usize;
        for i in 0..nrows {
            let row = rows.add(i * ROW_STRIDE);
            if *row == 0 || !forms::kept(vanilla, i) {
                continue;
            }
            let ty = *(row.add(ROW_TYPE) as *const i32);
            let mut ok = false;
            if tab_ok {
                for k in 0..10 {
                    let v = *tab.add(k);
                    if v == -1 {
                        break;
                    }
                    if v == ty {
                        ok = true;
                        break;
                    }
                }
            }
            if ok {
                kept += 1;
            }
        }
        kept
    }
}

/// The type-tab rebuild wrapper: dump in, run the original, dump out.
unsafe extern "system" fn my_tab_rebuild(manager: *mut u8, tab: *const i32) {
    unsafe {
        let Some(m) = G.get() else { return };
        let Ok(mut g) = m.lock() else { return };
        let Inner {
            logger,
            manager: mgr,
            tab_tramp,
            tab: cur,
            ..
        } = &mut *g;
        cur.calls += 1;
        let call = cur.calls;
        let now = sizes(*mgr);
        if !cur.seen || now != cur.last {
            dump(logger, *mgr, cur, "tab rebuild", call);
            if cur.seen {
                delta(logger, *mgr, cur, "tab rebuild");
            } else {
                cur.prev = vec_contents(*mgr, VEC_FILTERED, MAX_ROWS_DUMPED);
            }
            cur.last = now;
            cur.seen = true;
            if cur.full_left > 0 {
                cur.full_left -= 1;
            }
        }
        let Some(orig) = tab_tramp.as_ref().map(|t| t.as_ptr()) else {
            return;
        };
        let f: TabFn = core::mem::transmute(orig);
        drop(g);
        f(manager, tab);
        let Ok(mut g) = m.lock() else { return };
        let Inner {
            logger,
            manager: mgr,
            tab_table,
            vanilla,
            tab: cur,
            ..
        } = &mut *g;
        let now = sizes(*mgr);
        if now != cur.last {
            dump(logger, *mgr, cur, "tab rebuild", call);
            delta(logger, *mgr, cur, "tab rebuild");
            cur.last = now;
            if cur.full_left > 0 {
                cur.full_left -= 1;
            }
        }
        // the mechanism + fix evidence line
        let tab_idx = if *tab_table != 0 && (tab as usize) >= *tab_table {
            (tab as usize - *tab_table) / TAB_STRIDE
        } else {
            0
        };
        let composed = vec_len(*mgr, VEC_FILTERED).unwrap_or(0);
        let vanilla_kept = vanilla_would_keep(*mgr, vanilla, tab);
        logger.line(&format!(
            "probe: tab rebuild #{call} tab=#{tab_idx} -> +0x48 {composed} rows kept by the \
             composed keep-set; the vanilla 97-scan would have kept {vanilla_kept}"
        ));
    }
}

/// The window wrapper: a compact frame bracket.
unsafe extern "system" fn my_window(window_obj: *mut c_void) {
    unsafe {
        let Some(m) = G.get() else { return };
        let Ok(mut g) = m.lock() else { return };
        let Inner {
            logger,
            manager: mgr,
            win: cur,
            win_tramp,
            ..
        } = &mut *g;
        cur.calls += 1;
        let call = cur.calls;
        let now = sizes(*mgr);
        if !cur.seen || now != cur.last {
            let [a, b, c] = now;
            logger.line(&format!(
                "probe: window #{call} (entry) lists +0x30={a} +0x48={b} +0x60={c}"
            ));
            cur.last = now;
            cur.seen = true;
        }
        let Some(orig) = win_tramp.as_ref().map(|t| t.as_ptr()) else {
            return;
        };
        let f: WindowFn = core::mem::transmute(orig);
        drop(g);
        f(window_obj);
        let Ok(mut g) = m.lock() else { return };
        let Inner {
            logger,
            manager: mgr,
            win: cur,
            ..
        } = &mut *g;
        let now = sizes(*mgr);
        if now != cur.last {
            let [a, b, c] = now;
            logger.line(&format!(
                "probe: window #{call} (exit) lists +0x30={a} +0x48={b} +0x60={c}"
            ));
            cur.last = now;
        }
    }
}
