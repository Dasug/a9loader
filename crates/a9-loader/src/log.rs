//! Tiny append-only log next to the exe (`a9loader.log`) — additive M0
//! diagnostics; the C reference is silent. M2 grows this into the structured
//! journal (`<mods_dir>/a9loader.log`). Never panics, never fails the boot.

use std::io::Write;
use std::path::PathBuf;

const MAX_BYTES: u64 = 1_000_000;

fn log_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    Some(dir.join("a9loader.log"))
}

pub fn line(s: &str) {
    let Some(path) = log_path() else { return };
    if let Ok(md) = std::fs::metadata(&path) {
        if md.len() > MAX_BYTES {
            let _ = std::fs::remove_file(&path);
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(s.as_bytes());
        let _ = f.write_all(b"\n");
    }
}
