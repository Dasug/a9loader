//! a9-loader — the `version.dll` proxy product for A-Train 9 v5 (Steam build
//! `5.00.5634`).
//!
//! **M0 scope**: a 1:1 behavioral port of the C reference proxy
//! (`version_proxy.c` in the research corpus, `projects/dll-proxy/`):
//! forwarded VERSION.dll exports + the train-list filter hook, driven by the
//! legacy `atrain9_proxy.ini` compat config. The general engine (build-id
//! gate, mod manifests, runtime overlay) lands with M2/M3 — see the
//! architecture plan in the corpus (`projects/mod-loader/PLAN.md`).
//!
//! Parity rules kept from the C reference:
//! - patches are applied in `DllMain` at `DLL_PROCESS_ATTACH` (the exe image
//!   is mapped by then; the file on disk stays vanilla — APP_ERR195);
//! - the real system VERSION.dll is loaded lazily on the first forwarded
//!   call, never inside `DllMain` (loader lock);
//! - every failure degrades silently to an unpatched (vanilla) game — the
//!   only additive behavior is the `a9loader.log` next to the exe.

#![allow(non_snake_case)]

// pure, host-testable; pub because the M2 engine consumes them from both
// targets and the host lib build has no other callers (avoids dead-code noise)
pub mod forms;
pub mod ini;

#[cfg(target_os = "windows")]
mod compat; // DllMain glue: mode dispatch (parity with version_proxy.c)
#[cfg(target_os = "windows")]
mod forward; // the 10 forwarded VERSION.dll exports
#[cfg(target_os = "windows")]
mod log; // exe-dir a9loader.log (additive; the C reference is silent)
#[cfg(target_os = "windows")]
mod patch; // VirtualProtect write helpers + patch recipes
#[cfg(target_os = "windows")]
mod sys; // minimal hand-declared Win32 (migrate to windows-sys in M2)

#[cfg(target_os = "windows")]
#[unsafe(no_mangle)]
unsafe extern "system" fn DllMain(
    hinst: *mut core::ffi::c_void,
    reason: u32,
    reserved: *mut core::ffi::c_void,
) -> i32 {
    unsafe { compat::dllmain(hinst.cast(), reason, reserved) }
}
