//! a9-loader — the `version.dll` proxy product for A-Train 9 v5.
//!
//! Scope: the general loader engine. `DllMain` boots the engine:
//! config → build identity (from the exe's `VS_VERSIONINFO`, parsed from the
//! file — the version APIs are our own forwarded exports and must not run
//! under the loader lock) → address-book gate → mods-dir scan → dependency
//! resolution → expect-verified, journaled effect execution. Mod manifests
//! are data (`mod.toml`, schema in the `a9-mod` crate); there is deliberately
//! no scripting in manifests. The original behaviors live on as the
//! `train_list`/`nop` handlers.
//!
//! Legacy `atrain9_proxy.ini` support was dropped before anything shipped
//! (the ini was never published to anyone); mods are `mods/<id>/mod.toml`.
//!
//! Hard rules:
//! - patches are applied in `DllMain` at `DLL_PROCESS_ATTACH`; the exe file on
//!   disk stays vanilla (APP_ERR195);
//! - the real system VERSION.dll is loaded lazily on the first forwarded
//!   call — never inside `DllMain` (loader lock);
//! - every failure degrades to an unpatched (vanilla) game + a loud log line.

#![allow(non_snake_case)]

pub mod forms; // pure: FORM list / allow-list parsing + bitmap (host-testable)

#[cfg(target_os = "windows")]
mod clamp; // the always-on save-id clamp guard
#[cfg(target_os = "windows")]
mod engine; // the boot engine (config, build gate, scan, resolve, execute)
#[cfg(target_os = "windows")]
mod forward; // the 10 forwarded VERSION.dll exports
#[cfg(target_os = "windows")]
mod formreadout; // the custom-train-form editor list fix
#[cfg(target_os = "windows")]
mod handlers; // effect handlers: train_list, nop, [[patch]] executor
#[cfg(target_os = "windows")]
mod injector; // pack_serving = "inject" — the famb window-scoped splice
#[cfg(target_os = "windows")]
mod log; // <mods_dir>/a9loader.log
#[cfg(target_os = "windows")]
mod overlay; // the CFile::Open redirect + merge cache
#[cfg(target_os = "windows")]
mod patch; // VirtualProtect write helpers + patch recipes
#[cfg(all(target_os = "windows", feature = "probe"))]
mod probe; // the runtime fetch probe (probe_fetch = true diagnostics)
#[cfg(all(target_os = "windows", feature = "probe"))]
mod probe_save; // the save-path writer probe (probe_save = true diagnostics)
#[cfg(all(target_os = "windows", feature = "probe"))]
mod probe_window; // the purchase-window list dump (probe_window = true diagnostics)
#[cfg(target_os = "windows")]
mod rowinject; // direct spec-row injection (mechanism + harness export; NOT boot-wired)
#[cfg(target_os = "windows")]
mod sys; // minimal hand-declared Win32 + readability guards
#[cfg(target_os = "windows")]
mod tabrebuild; // the type-tab rebuild fix
#[cfg(target_os = "windows")]
mod tailgate; // the family-A in-place decode append ([[asset]] serving)

#[cfg(target_os = "windows")]
mod workspace; // the family-A decode workspace swap (capacity ceiling)

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
