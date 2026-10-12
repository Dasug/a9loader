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

/// Splice an in-image call site over to a nominated stub, returning the
/// ORIGINAL call target (site+5+rel32) for the record. The site's `expect`
/// bytes (address book) are verified first — a mismatch refuses without
/// touching anything. When `expect_target` is `Some(va)` the computed
/// original target must equal it — refuse before any write (this pins the
/// runner identity: the call really is the code the address book names).
///
/// The nominated stub is an `extern "system"` function living in OUR dll —
/// usually many GB away from the game image, unreachable by any `E8`-rel32.
/// The splice therefore lands on a 23-byte VERNIER page allocated near the
/// game image:
///
/// ```text
/// 58                   pop rbx            ; rbx = return address (site+5)
/// 48 83 EC 20          sub rsp,0x20       ; 16-align + shadow for the call
/// 48 B8 <imm64 stub>   movabs rax, stub   ; patched at install
/// FF D0                call rax
/// 48 83 C4 20          add rsp,0x20
/// FF E3                jmp rbx            ; return where the call would have
/// ```
///
/// The original call site keeps its opcode byte (`E8`) and gets a fresh
/// rel32 into the vernier — always within rel32 range by construction (the
/// page is allocated within ±2 GB of the site, first choice
/// site+0x10000000). Call semantics match the original call exactly: the
/// caller's volatile-argument registers (e.g. RCX = pool pointer) reach the
/// stub unchanged, the stub's own callee-saved register preservation keeps
/// the caller's live EBX/RSI/RBP/R12B intact (the loader reads EBX in the
/// instruction right after the spliced site), and `jmp rbx` returns to
/// site+5 — the exact continuation the original call had.
pub unsafe fn splice_call(
    site: *mut u8,
    expect: &[u8],
    stub: *const c_void,
    prot: DWORD,
    expect_target: Option<usize>,
    cave: *mut u8,
    cave_expect: &[u8],
) -> Result<usize, String> {
    unsafe {
        if expect.len() < 5 {
            return Err("expect bytes shorter than the 5 call bytes".into());
        }
        let site_va = site as usize;
        // verify the recorded bytes (VirtualProtect round-trip like
        // install_trampoline)
        let mut old: DWORD = 0;
        if VirtualProtect(site.cast(), expect.len(), prot, &mut old) == 0 {
            return Err("VirtualProtect on the call site failed".into());
        }
        let ok = std::slice::from_raw_parts(site, expect.len()) == expect;
        VirtualProtect(site.cast(), expect.len(), old, &mut old);
        if !ok {
            return Err(
                "call-site bytes differ from the address book (game update?) — \
                 refusing to splice"
                    .into(),
            );
        }
        if *site != 0xE8 {
            return Err("the site is not an E8 call (book drift?)".into());
        }
        let rel = i32::from_le_bytes([
            *site.wrapping_add(1),
            *site.wrapping_add(2),
            *site.wrapping_add(3),
            *site.wrapping_add(4),
        ]);
        let original = site_va.wrapping_add(5).wrapping_add(rel as isize as usize);
        if let Some(want) = expect_target {
            if original != want {
                return Err(format!(
                    "original call target {original:#x} differs from the pinned \
                     target {want:#x} — refusing to splice"
                ));
            }
        }
        // the relay page: a recorded code CAVE inside the same image (a
        // compiler alignment pad — CC run), expect-verified like the site.
        // WHY: only 5 bytes at the site fit, so an E8 rel32 is the only
        // encoding — and a rel32 can't reach our loaded dll (wine/Windows
        // memory is far outside ±2 GiB). The cave -> hook relation uses the
        // absolute FF 25 + imm64 form instead:
        //   cave: [FF 25 00000000][8-byte stub address]
        //   site: E8 <rel32 = cave - (site + 5)>   (same image ⇒ always fits)
        // flow: game call → site → cave thunk → call [imm64] → our stub;
        // the stub returns to site+5 (the game's caller pushed it), so the
        // spliced call behaves like the original one byte-for-byte.
        if cave_expect.len() < 14 {
            return Err("cave shorter than the 14-byte relay thunk".into());
        }
        let mut told: DWORD = 0;
        if VirtualProtect(cave.cast(), cave_expect.len(), prot, &mut told) == 0 {
            return Err("VirtualProtect on the relay cave failed".into());
        }
        let cave_ok = std::slice::from_raw_parts(cave, cave_expect.len()) == cave_expect;
        VirtualProtect(cave.cast(), cave_expect.len(), old, &mut old);
        if !cave_ok {
            return Err(
                "relay cave bytes differ from the address book (game update?) — \
                 refusing to splice"
                    .into(),
            );
        }
        let mut thunk = [0u8; 14];
        thunk[0] = 0xFF;
        thunk[1] = 0x25;
        // [rip+0] — the immediate lands right after the instruction
        thunk[2..6].copy_from_slice(&0u32.to_le_bytes());
        thunk[6..14].copy_from_slice(&(stub as usize).to_ne_bytes());
        if !write_bytes(cave, &thunk, prot) {
            return Err("writing the relay thunk into the cave failed".into());
        }
        // the site: keep the E8 opcode byte, rewrite the 4 rel32 bytes only;
        // both the site and the cave live in the same image, so the rel32
        // always fits — assert it anyway
        let to_cave = (cave as usize).wrapping_sub(site_va + 5) as i64;
        if to_cave < i32::MIN as i64 || to_cave > i32::MAX as i64 {
            return Err(format!(
                "cave {:#x} out of rel32 range of the site {site_va:#x} — the \
                 book records them from the same image, this cannot happen",
                cave as usize
            ));
        }
        let mut write = [0u8; 5];
        write[0] = 0xE8;
        write[1..5].copy_from_slice(&(to_cave as i32).to_le_bytes());
        if !write_bytes(site, &write, prot) {
            return Err("writing the spliced rel32 failed".into());
        }
        Ok(original)
    }
}

/// A saved original function behind an installed hook: the prologue bytes
/// copied to executable memory, followed by a jump back to the original's
/// `entry + prologue_len` — calling `call` runs the original function.
///
/// SAFETY contract (caller's responsibility): the saved `prologue_len` bytes
/// must not be position-dependent (no rip-relative addressing) — pure stack
/// adjustment + register pushes — and `prologue_len` ≥ 12 so the hook's
/// `jmp [rip+0]; imm64` write fits over it. The cfile_open prologue
/// (push rbx/rbp/rsi/rdi/r14/r15; sub rsp,0x288 — 16 B) satisfies both.
pub struct Trampoline {
    /// the thunk's address, held as an integer (Send/Sync: the thunk is
    /// allocated once at boot and only ever called; it lives for the
    /// process lifetime — no free)
    thunk: usize,
}

impl Trampoline {
    /// The trampoline's entry — call it as the original function's type.
    pub fn as_ptr(&self) -> *const c_void {
        self.thunk as *const c_void
    }
}

/// Install `hook` over the original at `entry` and return a trampoline to
/// the original. `expect` (the address book's recorded original bytes) is
/// verified first — a mismatch refuses without touching anything.
pub unsafe fn install_trampoline(
    entry: *mut u8,
    hook: *const c_void,
    expect: &[u8],
    prot: DWORD,
) -> Result<Trampoline, String> {
    unsafe {
        if expect.is_empty() {
            return Err("no expect bytes recorded for this anchor".into());
        }
        if expect.len() < 12 {
            return Err("expect bytes shorter than the 12-byte hook write".into());
        }
        // verify the original prologue against the book
        let mut old: DWORD = 0;
        if VirtualProtect(entry.cast(), expect.len(), prot, &mut old) == 0 {
            return Err("VirtualProtect on the original entry failed".into());
        }
        let ok = std::slice::from_raw_parts(entry, expect.len()) == expect;
        VirtualProtect(entry.cast(), expect.len(), old, &mut old);
        if !ok {
            return Err(
                "original bytes at the anchor differ from the address book (game update?) — \
                 refusing to hook"
                    .into(),
            );
        }
        // thunk: [saved prologue][jmp [rip+0]; imm64 -> entry + prologue_len]
        // (the imm64 form — not E9 rel32: the allocator may place the thunk
        // far outside rel32 reach of the game image, e.g. under wine)
        let len = expect.len() + 14;
        let thunk = VirtualAlloc(
            std::ptr::null_mut(),
            len,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_EXECUTE_READWRITE,
        ) as *mut u8;
        if thunk.is_null() {
            return Err("VirtualAlloc for the trampoline failed".into());
        }
        std::ptr::copy_nonoverlapping(entry, thunk, expect.len());
        let target = entry.add(expect.len()) as usize;
        *thunk.add(expect.len()) = 0xFF;
        *thunk.add(expect.len() + 1) = 0x25;
        // +2..+6 stay 0 (rip-relative displacement 0)
        std::ptr::copy_nonoverlapping(
            target.to_le_bytes().as_ptr(),
            thunk.add(expect.len() + 6),
            8,
        );
        // hook over the entry
        if !write_bytes(entry, &jmp_bytes(hook), prot) {
            VirtualFree(thunk.cast(), 0, MEM_RELEASE);
            return Err("writing the hook over the original failed".into());
        }
        Ok(Trampoline {
            thunk: thunk as usize,
        })
    }
}
