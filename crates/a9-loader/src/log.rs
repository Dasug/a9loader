//! The loader log — `<mods_dir>/a9loader.log`, one line per event with a
//! local timestamp, size-capped, never panicking, never failing the boot.
//! M2 upgrade over the M0 stub: severity tags + timestamps + lazy path
//! creation (the mods dir may not exist yet when boot starts).

use crate::sys;
use std::io::Write;
use std::path::{Path, PathBuf};

const MAX_BYTES: u64 = 1_000_000;

#[derive(Clone)]
pub struct Logger {
    path: PathBuf,
    rotate_pending: bool,
}

impl Logger {
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            rotate_pending: true,
        }
    }

    fn emit(&self, tag: &str, msg: &str) {
        let Some(dir) = self.path.parent() else { return };
        let _ = std::fs::create_dir_all(dir);
        if self.rotate_pending {
            // checked once per run (Logger is shared per boot)
            if let Ok(md) = std::fs::metadata(&self.path) {
                if md.len() > MAX_BYTES {
                    let _ = std::fs::remove_file(&self.path);
                }
            }
        }
        let mut stamp = [0u16; 8];
        unsafe {
            let mut st = sys::SYSTEMTIME {
                wYear: 0,
                wMonth: 0,
                wDayOfWeek: 0,
                wDay: 0,
                wHour: 0,
                wMinute: 0,
                wSecond: 0,
                wMilliseconds: 0,
            };
            sys::GetLocalTime(&mut st);
            let f = |v| v;
            let parts = [
                f(st.wHour),
                f(st.wMinute),
                f(st.wSecond),
            ];
            write_two(&mut stamp[0..2], parts[0]);
            write_two(&mut stamp[2..4], parts[1]);
            write_two(&mut stamp[4..6], parts[2]);
        }
        let h = u16_to_str(&stamp[0..2]);
        let m = u16_to_str(&stamp[2..4]);
        let s = u16_to_str(&stamp[4..6]);
        let line = format!("[{h}:{m}:{s}] {tag}{msg}\n");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = f.write_all(line.as_bytes());
        }
    }

    pub fn line(&self, msg: &str) {
        self.emit("", msg);
    }
    /// debug-level line (only meaningful when the emitter checks trace flags;
    /// the overlay gates it on the config's log_level=debug)
    pub fn debug(&self, msg: &str) {
        self.emit("DBG:  ", msg);
    }
    pub fn warn(&self, msg: &str) {
        self.emit("WARN: ", msg);
    }
    pub fn err(&self, msg: &str) {
        self.emit("ERR:  ", msg);
    }
}

fn write_two(dst: &mut [u16], v: u16) {
    dst[0] = u16::from(b'0') + (v / 10) % 10;
    dst[1] = u16::from(b'0') + v % 10;
}

fn u16_to_str(digits: &[u16]) -> String {
    digits.iter().map(|&d| char::from_u32(d as u32).unwrap_or('?')).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_digit_formatting() {
        let mut buf = [0u16; 2];
        write_two(&mut buf, 7);
        assert_eq!(u16_to_str(&buf), "07");
        write_two(&mut buf, 42);
        assert_eq!(u16_to_str(&buf), "42");
    }
}
