//! The M6 `[[loose_asset]]` table — game files the game opens BY PATH (not
//! content inside a pack). Each payload is materialized into
//! `<mods_dir>/cache/loose/` at install time (a REAL file — `CFile::Read`/
//! `GetLength` are plain Win32 on the handle, so there is no memory-source
//! branch to exploit) and every `CFile::Open` is tested against the compiled
//! patterns BEFORE the pack logic; on a match the open path is rewritten to
//! the cache file. The table is scanned only when it is non-empty.

use super::{CFileOpenFn, Inner};
use crate::log;
use std::path::{Path, PathBuf};

/// One materialized `[[loose_asset]]` redirect: a compiled pattern and the
/// loader-cache file whose bytes must be served in its place.
pub(super) struct LooseEntry {
    mod_id: String,
    pattern: a9_mod::loose::Pattern,
    /// the cache file, relative to the game CWD (`mods\cache\loose\<name>`)
    cache_rel: PathBuf,
}

/// Serve a matching `[[loose_asset]]` copy in place of `norm`, or `None` when
/// the table is empty or nothing matches (the caller then proceeds with the
/// pack logic). `norm` is already normalized (lowercase + backslashes), which
/// is exactly what the pattern wants.
pub(super) unsafe fn try_serve(
    inner: &Inner,
    norm: &str,
    original: CFileOpenFn,
    this: *mut core::ffi::c_void,
    flags: u32,
    perr: *mut core::ffi::c_void,
) -> Option<i32> {
    if inner.packs.loose.is_empty() {
        return None;
    }
    let entry = inner.packs.loose.iter().find(|l| l.pattern.matches_norm(norm))?;
    if inner.trace_opens {
        inner.logger.debug(&format!(
            "loose match: {norm} -> {} (mod {})",
            entry.cache_rel.display(),
            entry.mod_id
        ));
    }
    let wide: Vec<u16> = entry
        .cache_rel
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    Some(unsafe { original(this, wide.as_ptr(), flags, perr) })
}

/// Materialize the `[[loose_asset]]` cache copies (M6).
///
/// For each op: read the payload from the mod dir, write it to
/// `<mods_dir>/cache/loose/<name>` (overwritten every boot — it is a cache),
/// and compile its pattern into the table the hook scans. The name is the mod
/// id + a sanitized pattern so two mods can never collide.
///
/// FAIL-OPEN per asset, always: an unreadable payload, a failed write, or an
/// unwritable cache dir logs one loud line naming the mod + pattern and drops
/// that asset; the game's original open then proceeds untouched.
pub(super) fn materialize_loose(
    ops: &[super::LooseOpPlan],
    mods_dir_abs: &Path,
    mods_dir_rel: &Path,
    logger: &log::Logger,
) -> Vec<LooseEntry> {
    if ops.is_empty() {
        return Vec::new();
    }
    let dir = mods_dir_abs.join("cache").join("loose");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        logger.err(&format!(
            "loose: cannot create {}: {e} — {} loose asset(s) dropped (fail-open)",
            dir.display(),
            ops.len()
        ));
        return Vec::new();
    }
    let mut out: Vec<LooseEntry> = Vec::with_capacity(ops.len());
    let mut used: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for op in ops {
        let payload = match std::fs::read(op.mod_dir.join(&op.file)) {
            Ok(b) => b,
            Err(e) => {
                logger.err(&format!(
                    "loose: mod {} pattern \"{}\": payload {} unreadable ({e}) — dropped \
                     (fail-open)",
                    op.mod_id, op.pattern, op.file
                ));
                continue;
            }
        };
        let name = unique_loose_name(&op.mod_id, &op.pattern, &mut used);
        let abs = dir.join(&name);
        if let Err(e) = std::fs::write(&abs, &payload) {
            logger.err(&format!(
                "loose: mod {} pattern \"{}\": writing {}: {e} — dropped (fail-open)",
                op.mod_id,
                op.pattern,
                abs.display()
            ));
            continue;
        }
        let cache_rel = mods_dir_rel.join("cache").join("loose").join(&name);
        let pattern = a9_mod::loose::Pattern::compile(&op.pattern);
        logger.line(&format!(
            "loose: mod {} pattern \"{}\" -> {} ({} B)",
            op.mod_id,
            pattern.text(),
            cache_rel.display(),
            payload.len()
        ));
        out.push(LooseEntry {
            mod_id: op.mod_id.clone(),
            pattern,
            cache_rel,
        });
    }
    out
}

/// The cache file name for one loose asset: `<mod-id>__<sanitized pattern>`
/// (two mods can never collide), disambiguated with a counter if two distinct
/// patterns in one mod sanitize to the same string.
fn unique_loose_name(
    mod_id: &str,
    pattern: &str,
    used: &mut std::collections::BTreeSet<String>,
) -> String {
    let base = format!(
        "{}__{}",
        sanitize_name(mod_id),
        sanitize_name(&a9_mod::loose::normalize(pattern))
    );
    let mut name = base.clone();
    let mut n = 2u32;
    while !used.insert(name.clone()) {
        name = format!("{base}__{n}");
        n += 1;
    }
    name
}

/// A cache-name-safe form of a string: keep `[a-z0-9._-]`, map the rest to
/// `_`, and bound the length (Windows path components are 255 chars).
fn sanitize_name(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    out.truncate(100);
    out
}
