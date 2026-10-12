//! a9-loader — the `version.dll` proxy product for A-Train 9 v5.
//!
//! **M2 scope**: the general loader engine. `DllMain` boots the engine:
//! config → build identity (from the exe's `VS_VERSIONINFO`, parsed from the
//! file — the version APIs are our own forwarded exports and must not run
//! under the loader lock) → address-book gate → mods-dir scan → dependency
//! resolution → expect-verified, journaled effect execution. Mod manifests
//! are data (`mod.toml`, schema in the `a9-mod` crate); there is deliberately
//! no scripting in manifests. The M0 parity behaviors live on as the
//! `train_list`/`nop` handlers.
//!
//! Legacy `atrain9_proxy.ini` support was dropped before anything shipped
//! (the ini was never published to anyone); mods are `mods/<id>/mod.toml`.
//!
//! Hard rules kept from the corpus handover:
//! - patches are applied in `DllMain` at `DLL_PROCESS_ATTACH`; the exe file on
//!   disk stays vanilla (APP_ERR195);
//! - the real system VERSION.dll is loaded lazily on the first forwarded
//!   call — never inside `DllMain` (loader lock);
//! - every failure degrades to an unpatched (vanilla) game + a loud log line.

#![allow(non_snake_case)]

pub mod forms; // pure: FORM list / allow-list parsing + bitmap (host-testable)

#[cfg(target_os = "windows")]
mod engine; // the M2 boot engine (config, build gate, scan, resolve, execute)
#[cfg(target_os = "windows")]
mod forward; // the 10 forwarded VERSION.dll exports
#[cfg(target_os = "windows")]
mod handlers; // effect handlers: train_list, nop, [[patch]] executor
#[cfg(target_os = "windows")]
mod injector; // M4: pack_serving = inject — the famb window-scoped splice
#[cfg(target_os = "windows")]
mod log; // <mods_dir>/a9loader.log
#[cfg(target_os = "windows")]
mod overlay; // M3: the CFile::Open redirect + merge cache
#[cfg(target_os = "windows")]
mod patch; // VirtualProtect write helpers + patch recipes
#[cfg(target_os = "windows")]
mod probe; // M4: the runtime fetch probe (probe_fetch = true diagnostics)
#[cfg(target_os = "windows")]
mod sys; // minimal hand-declared Win32 + readability guards

#[cfg(target_os = "windows")]
#[unsafe(no_mangle)]
unsafe extern "system" fn DllMain(
    hinst: *mut core::ffi::c_void,
    reason: u32,
    _reserved: *mut core::ffi::c_void,
) -> i32 {
    if reason == sys::DLL_PROCESS_ATTACH {
        unsafe { engine::boot(hinst.cast()) };
    }
    1 // the game always boots
}
