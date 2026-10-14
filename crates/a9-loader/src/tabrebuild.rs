//! The purchase-window **type-tab cycle** fix (corpus notes/23).
//!
//! The big purchase/list window `train_list_big_list_window` (0x1402ca400)
//! rebuilds its list on every type tab (re)build by calling
//! `FUN_14019e760(&DAT_140e14510, &DAT_14068e950 + tab*0x2c)`. That function
//! re-derives row membership by linear-scanning the **vanilla** 97-entry
//! purchase whitelist (`g_dwWhitelist97` 0x1406fc1c0, bound 0x1406fc344):
//!
//! ```text
//! 14019e7ee: LEA RAX,[0x1406fc1c0]        ; the vanilla whitelist
//! 14019e7f5: CMP dword ptr [RAX],EDI      ; row index in it?
//! 14019e7f7: JZ  found
//! 14019e7f9: ADD RAX,4 ; CMP RAX,R13 ; JL 14019e7f5
//! 14019e802: JMP skip                     ; not found -> the row is dropped
//! ```
//!
//! It never consults the filter the loader replaces (`train_list_filter`
//! 0x1a13b0, used only by the boot-time builder), so every row the composed
//! filter admitted that is not in the vanilla 97 vanishes on the first cycle
//! and never comes back (the list shrinks, the scrollbar thumb grows).
//!
//! The fix rewrites exactly that 25-byte scan (0x14019e7ee..0x14019e807) to ask
//! the loader's **composed keep-bitmap** ([`crate::handlers::composed_allowed`])
//! instead of scanning the vanilla table:
//!
//! ```text
//! mov r13,r8 ; mov ecx,edi ; call <relay cave> ; mov r8,r13
//! test al,al ; jnz found ; jmp skip ; nops
//! ```
//!
//! `R13` is dead after this site (its whitelist-end value is read only by the
//! scan being replaced — the loop's later code never touches it before the
//! epilogue restores it), so it parks `R8` (the rows base, live across the
//! call) without touching the stack: the entry keeps the x64 ABI's `RSP ≡ 8`
//! discipline at the callee. The relay cave is the same `FF 25` + imm64
//! thunk the M4/M5 splices use, so the target can live anywhere.
//!
//! Everything else — the vector append, its capacity growth, the tab-type
//! walk, the row-validity test — stays the game's own code. Fail-open: any
//! anchor/expect mismatch refuses without a write and the window keeps its
//! vanilla behaviour.

use crate::{forms, handlers, patch, sys};
use a9_mod::builds::BuildEntry;
use core::ffi::c_void;

/// The vanilla-whitelist scan inside FUN_14019e760 (25 bytes).
pub const ANCHOR_SCAN: &str = "train_list_tab_scan";
/// The same-image relay cave the rewritten scan calls.
pub const ANCHOR_CAVE: &str = "train_list_tab_cave";

/// The rewritten scan's length (the original region is exactly this long).
const SCAN_LEN: usize = 25;
/// The branch targets the rewrite must reproduce, as offsets from the scan
/// start: `found` = 0x14019e807 (+25), `skip` = 0x14019e8a3 (+0xb5).
const FOUND_OFF: usize = 25;
const SKIP_OFF: usize = 0xb5;

/// The composed-keep test the rewritten scan calls: 1 = the row survives the
/// loader's composed filter (the vanilla whitelist is bypassed). With no
/// composed bitmap the patch is never installed, but answer "keep" defensively
/// rather than emptying the list.
unsafe extern "system" fn tab_membership(id: u32) -> u8 {
    match handlers::composed_allowed() {
        Some(bm) => u8::from(forms::kept(bm, id as usize)),
        None => 1,
    }
}

/// Install the type-tab cycle fix. Expect-verifies the scan region AND the
/// relay cave before any write; a mismatch refuses loudly and leaves the game
/// vanilla.
pub unsafe fn install(entry: &BuildEntry, base: *const u8) -> Result<String, String> {
    unsafe {
        let scan = entry
            .anchor(ANCHOR_SCAN)
            .ok_or_else(|| format!("this build has no {ANCHOR_SCAN} anchor"))?;
        let cave = entry
            .anchor(ANCHOR_CAVE)
            .ok_or_else(|| format!("this build has no {ANCHOR_CAVE} anchor"))?;
        let scan_expect = scan.expect_bytes().unwrap_or_default();
        let cave_expect = cave.expect_bytes().unwrap_or_default();
        if scan_expect.len() < SCAN_LEN {
            return Err(format!(
                "{ANCHOR_SCAN} records {} expect bytes, the scan needs {SCAN_LEN}",
                scan_expect.len()
            ));
        }
        let site = base as usize + scan.rva as usize;
        let cave_addr = base as usize + cave.rva as usize;
        if !sys::is_readable(site as *const c_void, SCAN_LEN) {
            return Err(format!("{ANCHOR_SCAN} rva {:#x} is not readable memory", scan.rva));
        }
        if std::slice::from_raw_parts(site as *const u8, SCAN_LEN) != &scan_expect[..SCAN_LEN] {
            return Err(format!(
                "{ANCHOR_SCAN} bytes differ from the address book (game update?) — refusing"
            ));
        }
        // the relay cave: FF 25 + imm64 -> the composed-keep test
        patch::install_relay_cave(
            cave_addr as *mut u8,
            &cave_expect,
            tab_membership as *const c_void,
            sys::PAGE_EXECUTE_READWRITE,
        )?;
        // the rewritten scan
        let mut b = [0u8; SCAN_LEN];
        //   mov r13, r8            (park the rows base; R13 is dead after here)
        b[0] = 0x4d;
        b[1] = 0x89;
        b[2] = 0xc5;
        //   mov ecx, edi           (the row index = form id)
        b[3] = 0x89;
        b[4] = 0xf9;
        //   call rel32 -> the cave
        b[5] = 0xe8;
        let rel = cave_addr as i64 - (site as i64 + 10);
        if rel < i32::MIN as i64 || rel > i32::MAX as i64 {
            return Err(format!(
                "cave {cave_addr:#x} out of rel32 range of the scan {site:#x} — the \
                 book records them from the same image, this cannot happen"
            ));
        }
        b[6..10].copy_from_slice(&(rel as i32).to_le_bytes());
        //   mov r8, r13
        b[10] = 0x4d;
        b[11] = 0x89;
        b[12] = 0xe8;
        //   test al, al
        b[13] = 0x84;
        b[14] = 0xc0;
        //   jnz found   (short form reaches +25)
        b[15] = 0x75;
        b[16] = (FOUND_OFF - 17) as u8;
        //   jmp skip
        b[17] = 0xe9;
        let skip_rel = SKIP_OFF as i64 - 22;
        b[18..22].copy_from_slice(&(skip_rel as i32).to_le_bytes());
        //   nops to the end of the original region
        b[22] = 0x90;
        b[23] = 0x90;
        b[24] = 0x90;
        if !patch::write_bytes(site as *mut u8, &b, sys::PAGE_EXECUTE_READWRITE) {
            return Err("writing the rewritten scan failed".into());
        }
        Ok(format!(
            "tab cycle fix: scan rva={:#x} -> the composed keep-set (expect=verified, \
             vanilla 97-scan bypassed); cave rva={:#x}",
            scan.rva, cave.rva
        ))
    }
}
