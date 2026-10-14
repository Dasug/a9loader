//! The **custom-train-form editor** candidate-list fix.
//!
//! The second vanilla-whitelist scan (VA 0x14019e950), whose
//! sole caller is `train_form_readout_refresh` (0x1401f3ec0), rebuilds the
//! 0x18-stride candidate vector at `manager+0x34e8` (manager =
//! 0x140e14510, so the vector lives at 0x140e179f8). That vector is the
//! list the **custom train-form editor** window renders (ui_mode_apply case
//! 0xb, window singleton 0x140e24d50 — the 車両編成数 / "Train
//! Configuration" screen with the 全て/オリジナル/インポート sub-tabs).
//!
//! For the base-form sub-views (the `All` and `Original` tabs) the function
//! walks the 0x140-byte spec rows (`manager+0x18`) and, for each **valid** row
//! (`row[0] != 0`), linear-scans the **vanilla** 97-entry purchase whitelist
//! (`g_dwWhitelist97` 0x1406fc1c0, bound 0x1406fc344):
//!
//! ```text
//! 14019e9df: LEA RAX,[0x1406fc1c0]        ; the vanilla whitelist
//! 14019e9e6: CMP dword ptr [RAX],EBX      ; row index in it?
//! 14019e9e8: JZ  0x14019ea52              ; found -> the primary admit path
//! 14019e9ea: ADD RAX,4 ; CMP RAX,RCX ; JL 14019e9e6
//! 14019e9f3: ...                          ; NOT found -> the fallback path:
//!                                         ;   skipped unless 特殊車両フラグ
//!                                         ;   (row+0x9e) >= 2 or the id is in
//!                                         ;   the NT3000-custom ranges
//! ```
//!
//! It never consults the filter the loader replaces (`train_list_filter`
//! 0x1a13b0, used only by the boot-time builder), so every mod train the
//! composed filter admitted that is not in the vanilla 97 (and is not a
//! 特殊車両, `row+0x9e >= 2`) is missing from the editor's base-train list.
//!
//! The fix rewrites exactly that 20-byte scan (0x14019e9df..0x14019e9f3) to ask
//! the loader's **composed keep-bitmap** ([`crate::handlers::composed_allowed`])
//! instead of scanning the vanilla table:
//!
//! ```text
//! mov ecx,ebx ; call <relay cave> ; xor r11d,r11d ; test al,al
//! jnz found(+0x73) ; nops to the region end
//! ```
//!
//! `R11` is 0 here and is read by the primary admit path (`mov edx,r11d` at
//! 0x14019ea52), so it is re-zeroed right after the call; `RBX` (the row
//! index) and `RDI` (the row pointer) are callee-saved and survive the call
//! untouched. `RAX`/`RCX` are the scan's own cursor/bound (dead after it).
//! `RSP` is 16-aligned at the site (the prologue pushes 8 registers then
//! `sub rsp,0x98`), so the `call` presents the ABI's `RSP ≡ 8` at the callee.
//!
//! The "not found" case is the region's fall-through (offset +0x14 = the
//! original 0x14019e9f3 fallback path), so no second branch is needed.
//! Everything else — the row walk, the type-tab/mode filters, the special
//! ranges, the vector append — stays the game's own code.
//!
//! Fail-open: any anchor/expect mismatch refuses without a write, and the
//! patch is only installed when a `train_list` mod composed a filter (so the
//! vanilla behaviour is untouched otherwise).

use crate::{forms, handlers, patch, sys};
use a9_mod::builds::BuildEntry;
use core::ffi::c_void;

/// The vanilla-whitelist scan inside the second scan function (20 bytes).
pub const ANCHOR_SCAN: &str = "train_form_scan";
/// The same-image relay cave the rewritten scan calls.
pub const ANCHOR_CAVE: &str = "train_form_cave";

/// The rewritten scan's length (the original region is exactly this long).
const SCAN_LEN: usize = 20;
/// The `found` branch target as an offset from the scan start: 0x14019ea52 is
/// the primary admit path (+0x73). The "not found" case is the region's own
/// fall-through at +0x14 (0x14019e9f3).
const FOUND_OFF: usize = 0x73;
/// The `call` opcode sits at offset 2; its instruction ends at offset 7.
const CALL_OFF: usize = 2;

/// The composed-keep test the rewritten scan calls: 1 = the row survives the
/// loader's composed filter (the vanilla whitelist is bypassed). With no
/// composed bitmap the patch is never installed, but answer "keep" defensively
/// rather than emptying the list.
unsafe extern "system" fn form_membership(id: u32) -> u8 {
    match handlers::composed_allowed() {
        Some(bm) => u8::from(forms::kept(bm, id as usize)),
        None => 1,
    }
}

/// Install the custom-train-form editor fix. Expect-verifies the scan region
/// AND the relay cave before any write; a mismatch refuses loudly and leaves
/// the game vanilla.
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
            form_membership as *const c_void,
            sys::PAGE_EXECUTE_READWRITE,
        )?;
        // the rewritten scan
        let mut b = [0u8; SCAN_LEN];
        //   mov ecx, ebx           (the row index = form id)
        b[0] = 0x89;
        b[1] = 0xd9;
        //   call rel32 -> the cave
        b[CALL_OFF] = 0xe8;
        let rel = cave_addr as i64 - (site as i64 + CALL_OFF as i64 + 5);
        if rel < i32::MIN as i64 || rel > i32::MAX as i64 {
            return Err(format!(
                "cave {cave_addr:#x} out of rel32 range of the scan {site:#x} — the \
                 book records them from the same image, this cannot happen"
            ));
        }
        b[3..7].copy_from_slice(&(rel as i32).to_le_bytes());
        //   xor r11d, r11d         (R11 is read as 0 by the primary admit path)
        b[7] = 0x45;
        b[8] = 0x31;
        b[9] = 0xdb;
        //   test al, al
        b[10] = 0x84;
        b[11] = 0xc0;
        //   jnz found   (short form reaches +0x73)
        b[12] = 0x75;
        b[13] = (FOUND_OFF - 14) as u8;
        //   nops to the end of the original region (fall through to +0x14)
        for byte in b.iter_mut().skip(14) {
            *byte = 0x90;
        }
        if !patch::write_bytes(site as *mut u8, &b, sys::PAGE_EXECUTE_READWRITE) {
            return Err("writing the rewritten scan failed".into());
        }
        Ok(format!(
            "form readout fix: scan rva={:#x} -> the composed keep-set (expect=verified, \
             vanilla 97-scan bypassed); cave rva={:#x}",
            scan.rva, cave.rva
        ))
    }
}
