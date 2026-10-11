//! In-memory patch recipes — direct ports of the C reference's writers.
//! Every write: VirtualProtect → write → restore → FlushInstructionCache.

use crate::sys::*;
use core::ffi::c_void;

/// The C nop stub for the filter: `mov [rcx], r8; mov rax, rcx; ret` + nops —
/// an identity `remove_if` (keeps all valid forms).
pub const NOP_STUB: [u8; 16] = [
    0x4C, 0x89, 0x01, 0x48, 0x89, 0xC8, 0xC3, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
    0x90,
];

/// `jmp qword ptr [rip+0] ; <imm64>` + 2 nops (16 bytes) — the C hook write.
pub fn jmp_bytes(dest: *const c_void) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[0] = 0xFF;
    b[1] = 0x25;
    // b[2..6] stays 0 (rip-relative displacement)
    b[6..14].copy_from_slice(&(dest as usize).to_ne_bytes());
    b[14] = 0x90;
    b[15] = 0x90;
    b
}

/// VirtualProtect-write-restore-flush. Returns `false` (nothing written) when
/// VirtualProtect fails — the C reference's silent-return behavior; callers
/// log it.
pub unsafe fn write_bytes(addr: *mut u8, bytes: &[u8], prot: DWORD) -> bool {
    unsafe {
        let mut old: DWORD = 0;
        if VirtualProtect(addr.cast(), bytes.len(), prot, &mut old) == 0 {
            return false;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), addr, bytes.len());
        VirtualProtect(addr.cast(), bytes.len(), old, &mut old);
        FlushInstructionCache(GetCurrentProcess(), addr.cast(), bytes.len());
        true
    }
}
