//! The 10 VERSION.dll exports the C reference forwards, forwarded to the real
//! system DLL. Lazy: the real DLL is loaded on the first forwarded call —
//! never inside `DllMain` (loader lock), exactly like the C `real()` helper.
//!
//! Signatures mirror the real APIs (parameter types are ABI-equivalent;
//! the version-info structs pass by pointer). On failure (real DLL/function
//! missing) we return 0, like the C.

use crate::sys::*;
use core::ffi::c_void;
use std::sync::OnceLock;

static REAL: OnceLock<usize> = OnceLock::new();

unsafe fn real_module() -> HMODULE {
    unsafe {
        *REAL.get_or_init(|| {
            let mut sys = [0u16; 264];
            let n = GetSystemDirectoryW(sys.as_mut_ptr(), sys.len() as u32) as usize;
            if n == 0 || n + 13 >= sys.len() {
                return 0;
            }
            let mut path = Vec::with_capacity(n + 13);
            path.extend_from_slice(&sys[..n]);
            path.extend(b"\\version.dll".iter().map(|&b| u16::from(b)));
            path.push(0);
            LoadLibraryW(path.as_ptr()) as usize
        }) as HMODULE
    }
}

fn real(name: &str) -> Option<FARPROC> {
    unsafe {
        let module = real_module();
        if module.is_null() {
            return None;
        }
        let mut buf = [0u8; 64];
        if name.len() >= buf.len() {
            return None; // export names are short ASCII; unreachable in practice
        }
        buf[..name.len()].copy_from_slice(name.as_bytes());
        let p = GetProcAddress(module, buf.as_ptr());
        if p.is_null() {
            None
        } else {
            Some(p)
        }
    }
}

macro_rules! forwarder {
    ($name:ident, ($($arg:ident: $ty:ty),*) -> $ret:ty) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn $name($($arg: $ty),*) -> $ret {
            unsafe {
                let Some(f) = real(stringify!($name)) else { return 0 as $ret };
                let f: unsafe extern "system" fn($($ty),*) -> $ret = std::mem::transmute(f);
                f($($arg),*)
            }
        }
    };
}

forwarder!(VerifyVersionInfoW, (a: *mut c_void, b: u32, c: u64) -> i32);
forwarder!(VerifyVersionInfoA, (a: *mut c_void, b: u32, c: u64) -> i32);
forwarder!(GetFileVersionInfoSizeW, (a: *const u16, b: *mut u32) -> u32);
forwarder!(GetFileVersionInfoSizeA, (a: *const u8, b: *mut u32) -> u32);
forwarder!(GetFileVersionInfoW, (a: *const u16, b: u32, c: u32, d: *mut c_void) -> i32);
forwarder!(GetFileVersionInfoA, (a: *const u8, b: u32, c: u32, d: *mut c_void) -> i32);
forwarder!(VerQueryValueW, (a: *const c_void, b: *const u16, c: *mut *mut c_void, d: *mut u32) -> i32);
forwarder!(VerQueryValueA, (a: *const c_void, b: *const u8, c: *mut *mut c_void, d: *mut u32) -> i32);
forwarder!(VerLanguageNameW, (a: u32, b: *mut u16, c: u32) -> u32);
forwarder!(VerLanguageNameA, (a: u32, b: *mut u8, c: u32) -> u32);
