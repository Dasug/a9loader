//! Minimal hand-declared Win32 surface for the M0 port — exactly the
//! functions the C reference (`version_proxy.c`) uses, minus what std already
//! covers (files via `std::fs`, exe path via `std::env::current_exe`).
//! M2 migrates this to the `windows-sys` crate once the engine's API surface
//! grows beyond this list.

use core::ffi::c_void;

pub type BOOL = i32;
pub type DWORD = u32;
pub type UINT = u32;
pub type HANDLE = *mut c_void;
pub type HMODULE = *mut c_void;
pub type FARPROC = *mut c_void;
pub type LPCSTR = *const u8;
pub type LPCWSTR = *const u16;

pub const TRUE: BOOL = 1;
pub const DLL_PROCESS_ATTACH: DWORD = 1;
pub const PAGE_READWRITE: DWORD = 0x04;
pub const PAGE_EXECUTE_READWRITE: DWORD = 0x40;

unsafe extern "system" {
    pub fn DisableThreadLibraryCalls(h: HMODULE) -> BOOL;
    pub fn GetModuleHandleW(name: LPCWSTR) -> HMODULE;
    pub fn LoadLibraryW(name: LPCWSTR) -> HMODULE;
    pub fn GetProcAddress(h: HMODULE, name: LPCSTR) -> FARPROC;
    pub fn GetSystemDirectoryW(buf: *mut u16, len: UINT) -> UINT;
    pub fn VirtualProtect(
        addr: *mut c_void,
        size: usize,
        new_prot: DWORD,
        old: *mut DWORD,
    ) -> BOOL;
    pub fn FlushInstructionCache(process: HANDLE, addr: *const c_void, size: usize) -> BOOL;
    pub fn GetCurrentProcess() -> HANDLE;
}
