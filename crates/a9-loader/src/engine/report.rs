//! The boot's end-of-run report lines and the `[[file]]` copy journal.
//!
//! The loader's only on-disk record of a `[[file]]` copy is
//! `<mods_dir>/journal.json` (JSON lines); [`copy_file_effect`] performs the
//! copy and writes it. [`result_line`] is the final count line the engine logs
//! once the whole boot is done.

use crate::log;
use std::path::Path;

/// `[[file]]` — copy a genuinely-new file into the game tree, journaled to
/// `<mods_dir>/journal.json` (JSON lines). Idempotent: identical content is
/// a no-op; a differing existing file refuses loudly (never overwrites —
/// replacing shipped content is [[pack]]/[[csv]] territory).
pub(super) fn copy_file_effect(
    mod_id: &str,
    f: &a9_mod::manifest::FileSection,
    mod_dir: &Path,
    exe_dir: &Path,
    mods_dir: &Path,
    logger: &log::Logger,
) {
    let dest = exe_dir.join(&f.dest);
    let src = mod_dir.join(&f.file);
    let payload = match std::fs::read(&src) {
        Ok(b) => b,
        Err(e) => {
            logger.err(&format!(
                "mod '{mod_id}': file {} -> {}: source unreadable ({e}) — skipped",
                f.file,
                f.dest
            ));
            return;
        }
    };
    if dest.exists() {
        match std::fs::read(&dest) {
            Ok(existing) if existing == payload => {
                logger.line(&format!(
                    "mod '{mod_id}': file {} already in place (idempotent)",
                    f.dest
                ));
                return;
            }
            _ => {
                logger.err(&format!(
                    "mod '{mod_id}': file dest {} exists with different content — \
                     refusing to overwrite (delete it, or replace content via [[pack]]/[[csv]])",
                    f.dest
                ));
                return;
            }
        }
    }
    if let Some(parent) = dest.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            logger.err(&format!(
                "mod '{mod_id}': file {} -> {}: cannot create {}",
                f.file,
                f.dest,
                parent.display()
            ));
            return;
        }
    }
    match std::fs::write(&dest, &payload) {
        Ok(()) => {
            logger.line(&format!(
                "mod '{mod_id}': file {} -> {} copied (journaled)",
                f.file,
                f.dest
            ));
            journal_append(mods_dir, mod_id, f);
        }
        Err(e) => logger.err(&format!(
            "mod '{mod_id}': file {} -> {}: copy failed ({e})",
            f.file,
            f.dest
        )),
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn journal_append(mods_dir: &Path, mod_id: &str, f: &a9_mod::manifest::FileSection) {
    // JSON lines — no array juggling, appends stay atomic under a lock-free
    // single-writer boot
    let line = format!(
        "{{\"mod\":\"{}\",\"file\":\"{}\",\"dest\":\"{}\"}}\n",
        json_escape(mod_id),
        json_escape(&f.file),
        json_escape(&f.dest)
    );
    let path = mods_dir.join("journal.json");
    use std::io::Write;
    if let Ok(mut fh) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = fh.write_all(line.as_bytes());
    }
}

/// The boot's final count line: memory effects applied/refused + file copies.
pub(super) fn result_line(applied: usize, refused: usize, files: usize) -> String {
    format!(
        "result: {applied} memory effect(s) applied, {refused} refused, {} file cop{} — game continues",
        files,
        if files == 1 { "y" } else { "ies" },
    )
}
