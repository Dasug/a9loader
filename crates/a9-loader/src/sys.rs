//! Minimal hand-declared Win32 surface — exactly the functions the loader
//! needs, minus what std already covers (files via `std::fs`, exe path via
//! `std::env::current_exe`). M3 may migrate this to the `windows-sys` crate
//! when the overlay grows the API surface.
#![allow(non_camel_case_types)]

use core::ffi::c_void;

pub type BOOL = i32;
pub type DWORD = u32;
pub type WORD = u16;
pub type UINT = u32;
pub type SIZE_T = usize;
pub type HANDLE = *mut c_void;
pub type HMODULE = *mut c_void;
pub type FARPROC = *mut c_void;
pub type LPCSTR = *const u8;
pub type LPCWSTR = *const u16;

pub const DLL_PROCESS_ATTACH: DWORD = 1;
pub const PAGE_READWRITE: DWORD = 0x04;
pub const PAGE_EXECUTE_READWRITE: DWORD = 0x40;

/// VirtualQuery states / protections (readability probing)
pub const MEM_COMMIT: DWORD = 0x1000;
pub const PAGE_NOACCESS: DWORD = 0x01;
pub const PAGE_GUARD: DWORD = 0x100;

#[repr(C)]
pub struct MEMORY_BASIC_INFORMATION {
    pub BaseAddress: *mut c_void,
    pub AllocationBase: *mut c_void,
    pub AllocationProtect: DWORD,
    pub PartitionId: WORD,
    pub RegionSize: SIZE_T,
    pub State: DWORD,
    pub Protect: DWORD,
    pub Type: DWORD,
}

#[repr(C)]
pub struct SYSTEMTIME {
    pub wYear: WORD,
    pub wMonth: WORD,
    pub wDayOfWeek: WORD,
    pub wDay: WORD,
    pub wHour: WORD,
    pub wMinute: WORD,
    pub wSecond: WORD,
    pub wMilliseconds: WORD,
}

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
    pub fn VirtualQuery(
        addr: *const c_void,
        info: *mut MEMORY_BASIC_INFORMATION,
        len: SIZE_T,
    ) -> SIZE_T;
    pub fn GetLocalTime(st: *mut SYSTEMTIME);
}

/// True when every page of `[ptr, ptr+len)` is committed and readable.
/// Guards raw-RVA manifest targets (a typo'd rva must refuse loudly, never
/// crash the game with an access violation mid-boot).
pub unsafe fn is_readable(ptr: *const c_void, len: usize) -> bool {
    unsafe {
        if ptr.is_null() || len == 0 {
            return false;
        }
        let mut off = 0usize;
        while off < len {
            let p = (ptr as *const u8).add(off) as *const c_void;
            let mut info = MEMORY_BASIC_INFORMATION {
                BaseAddress: core::ptr::null_mut(),
                AllocationBase: core::ptr::null_mut(),
                AllocationProtect: 0,
                PartitionId: 0,
                RegionSize: 0,
                State: 0,
                Protect: 0,
                Type: 0,
            };
            if VirtualQuery(p, &mut info, core::mem::size_of::<MEMORY_BASIC_INFORMATION>())
                == 0
            {
                return false;
            }
            if info.State != MEM_COMMIT
                || info.Protect & PAGE_NOACCESS != 0
                || info.Protect & PAGE_GUARD != 0
                || info.Protect == 0
            {
                return false;
            }
            let step = info.RegionSize.max(1);
            off = off.saturating_add(step);
        }
        true
    }
}
