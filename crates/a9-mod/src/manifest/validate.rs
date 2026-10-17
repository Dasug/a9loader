//! Per-section checks behind [`Manifest::validate`](super::Manifest::validate).
//!
//! One small function per section, dispatched in schema order by [`validate`].
//! The `[[patch]]`/`[[hook]]` checks share one target-key set (a patch and a
//! hook writing the same target are a conflict), so [`validate`] owns that set
//! and threads it through both.

use super::{AssetSection, Manifest, TargetRef, HOST_AUTO};
use crate::hex;
use crate::ModError;
use std::collections::BTreeSet;

pub(super) fn validate(m: &Manifest) -> Result<(), ModError> {
    validate_identity(m)?;
    validate_lists(m)?;
    validate_self_conflict(m)?;

    let mut keys = BTreeSet::new();
    validate_patches(m, &mut keys)?;
    validate_hooks(m, &mut keys)?;

    validate_packs(m)?;
    validate_csvs(m)?;
    validate_loose_assets(m)?;
    validate_assets(m)?;
    Ok(())
}

/// `id` must be a lowercase slug.
fn validate_identity(m: &Manifest) -> Result<(), ModError> {
    let id = &m.id;
    if id.is_empty()
        || !id.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        || !id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'))
    {
        return Err(ModError::Invalid(format!(
            "id must be a lowercase slug [a-z0-9][a-z0-9._-]*, got {id:?}"
        )));
    }
    Ok(())
}

/// The id-list fields must not carry empty entries.
fn validate_lists(m: &Manifest) -> Result<(), ModError> {
    for (field, list) in [
        ("game_builds", &m.game_builds),
        ("supplies", &m.supplies),
        ("requires", &m.requires),
        ("conflicts", &m.conflicts),
    ] {
        if list.iter().any(|s| s.trim().is_empty()) {
            return Err(ModError::Invalid(format!(
                "{field} contains an empty entry"
            )));
        }
    }
    Ok(())
}

fn validate_self_conflict(m: &Manifest) -> Result<(), ModError> {
    if m.conflicts.iter().any(|c| c == &m.id) {
        return Err(ModError::Invalid(format!(
            "{} conflicts with itself",
            m.id
        )));
    }
    Ok(())
}

/// `[[patch]]` — unique target keys (shared with `[[hook]]`), valid `data` hex,
/// and the raw-target `expect` requirement.
fn validate_patches(m: &Manifest, keys: &mut BTreeSet<String>) -> Result<(), ModError> {
    for (i, p) in m.patch.iter().enumerate() {
        let key = p.target.key(p.offset);
        if !keys.insert(format!("patch:{key}")) {
            return Err(ModError::Invalid(format!(
                "two [[patch]] entries write the same target {key:?} (patch #{i})"
            )));
        }
        match hex::parse(&p.data) {
            Some(b) if !b.is_empty() => {}
            _ => {
                return Err(ModError::Invalid(format!(
                    "patch {key}: `data` must be non-empty valid hex"
                )));
            }
        }
        if let TargetRef::Raw { rva, expect } = &p.target {
            if hex::parse(expect).map_or(true, |b| b.is_empty()) {
                return Err(ModError::Invalid(format!(
                    "patch {key}: raw rva {rva:#x} target requires non-empty `expect` hex"
                )));
            }
        }
        if let Some(e) = &p.expect {
            if hex::parse(e).map_or(true, |b| b.is_empty()) {
                return Err(ModError::Invalid(format!(
                    "patch {key}: `expect` must be non-empty valid hex"
                )));
            }
        }
    }
    Ok(())
}

/// `[[hook]]` — unique target keys (shared with `[[patch]]`) and per-handler
/// parameter shapes.
fn validate_hooks(m: &Manifest, keys: &mut BTreeSet<String>) -> Result<(), ModError> {
    for (i, h) in m.hook.iter().enumerate() {
        let key = h.target.key(0);
        if !keys.insert(format!("hook:{key}")) {
            return Err(ModError::Invalid(format!(
                "two effects write the same target {key:?} (hook #{i})"
            )));
        }
        match h.handler.as_str() {
            "train_list" => {
                if h.list.as_deref().map_or(true, |s| s.trim().is_empty()) {
                    return Err(ModError::Invalid(format!(
                        "hook {key}: handler train_list requires `list` (a file in the mod dir)"
                    )));
                }
            }
            "nop" => {
                if h.mode.is_some() || h.list.is_some() {
                    return Err(ModError::Invalid(format!(
                        "hook {key}: handler nop takes no parameters"
                    )));
                }
            }
            other => {
                return Err(ModError::Invalid(format!(
                    "hook {key}: unknown handler {other:?} (known: train_list, nop)"
                )));
            }
        }
        if let TargetRef::Raw { rva, expect } = &h.target {
            if hex::parse(expect).is_none() {
                return Err(ModError::Invalid(format!(
                    "hook {key}: raw rva {rva:#x} `expect` is not valid hex"
                )));
            }
        }
    }
    Ok(())
}

/// `[[pack]]` — family-B entry overlays: a lowercase-slug pack name, one
/// section per pack, not csvp, at least one entry, unique entry names and a
/// non-empty payload file per entry.
fn validate_packs(m: &Manifest) -> Result<(), ModError> {
    let mut pack_names = BTreeSet::new();
    for p in &m.pack {
        if !p
            .pack
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            || p.pack.is_empty()
        {
            return Err(ModError::Invalid(format!(
                "pack name must be a lowercase slug (e.g. \"mtentr0\"), got {:?}",
                p.pack
            )));
        }
        if !pack_names.insert(p.pack.clone()) {
            return Err(ModError::Invalid(format!(
                "two [[pack]] sections for pack {:?} — merge their entries into one",
                p.pack
            )));
        }
        if p.pack == "csvp" {
            return Err(ModError::Invalid(
                "csvp tables belong in [[csv]] sections, not [[pack]]".into(),
            ));
        }
        if p.entry.is_empty() {
            return Err(ModError::Invalid(format!(
                "[[pack]] {} has no [[pack.entry]]",
                p.pack
            )));
        }
        let mut names = BTreeSet::new();
        for e in &p.entry {
            if e.name.trim().is_empty() || e.name.contains('\0') {
                return Err(ModError::Invalid(format!(
                    "pack {} entry name must be a nonempty plain name, got {:?}",
                    p.pack, e.name
                )));
            }
            if !names.insert(e.name.clone()) {
                return Err(ModError::Invalid(format!(
                    "pack {} has two entries named {:?}",
                    p.pack, e.name
                )));
            }
            if e.file.trim().is_empty() {
                return Err(ModError::Invalid(format!(
                    "pack {} entry {:?} has no `file`",
                    p.pack, e.name
                )));
            }
        }
    }
    Ok(())
}

/// `[[csv]]` — csvp table overlays: a non-empty name, one section per table,
/// and a non-empty payload file.
fn validate_csvs(m: &Manifest) -> Result<(), ModError> {
    let mut csv_names = BTreeSet::new();
    for c in &m.csv {
        if c.name.trim().is_empty() || c.name.contains('\0') {
            return Err(ModError::Invalid(format!(
                "csv table name must be nonempty, got {:?}",
                c.name
            )));
        }
        if !csv_names.insert(c.name.clone()) {
            return Err(ModError::Invalid(format!(
                "two [[csv]] sections for table {:?}",
                c.name
            )));
        }
        if c.file.trim().is_empty() {
            return Err(ModError::Invalid(format!(
                "csv {:?} has no `file`",
                c.name
            )));
        }
    }
    Ok(())
}

/// `[[loose_asset]]` — game files opened by path, matched by a glob: a
/// non-empty pattern and payload file (mod-relative), one section per pattern
/// (case-insensitive).
fn validate_loose_assets(m: &Manifest) -> Result<(), ModError> {
    let mut loose_matches = BTreeSet::new();
    for l in &m.loose_asset {
        if l.match_.trim().is_empty() || l.match_.contains('\0') {
            return Err(ModError::Invalid(format!(
                "loose_asset match must be a nonempty path/glob, got {:?}",
                l.match_
            )));
        }
        if l.file.trim().is_empty() {
            return Err(ModError::Invalid(format!(
                "loose_asset {:?} has no `file`",
                l.match_
            )));
        }
        // the payload is a mod-relative file: no absolute path, no drive,
        // no `..` (same rule the other payload fields use)
        validate_mod_rel_path(&l.file, "loose_asset file")?;
        // two sections with the same pattern (case-insensitively) would
        // fight over one redirect — merge them into one
        if !loose_matches.insert(l.match_.to_ascii_lowercase()) {
            return Err(ModError::Invalid(format!(
                "two [[loose_asset]] sections match {:?} — one pattern is one redirect",
                l.match_
            )));
        }
    }
    Ok(())
}

/// `[[asset]]` — a family-A append + an i.dat row, single-file form
/// (`name`+`file`) or folder form (`folder`, expanded at scan time). Exactly
/// one form per section.
fn validate_assets(m: &Manifest) -> Result<(), ModError> {
    let mut asset_names = BTreeSet::new();
    for a in &m.asset {
        let folder = a.folder.as_deref().map(str::trim).filter(|s| !s.is_empty());
        match folder {
            Some(folder) => {
                if !a.name.trim().is_empty() || !a.file.trim().is_empty() {
                    return Err(ModError::Invalid(format!(
                        "asset folder {folder:?}: `name`/`file` are not allowed together \
                         with `folder` — a folder expands to one row per file"
                    )));
                }
                validate_mod_rel_path(folder, "asset folder")?;
            }
            None => {
                if a.name.trim().is_empty() {
                    return Err(ModError::Invalid(
                        "asset name must be a nonempty plain name \
                         (or use `folder` to declare a whole folder)"
                            .into(),
                    ));
                }
                if a.file.trim().is_empty() {
                    return Err(ModError::Invalid(format!(
                        "asset {:?} has no `file`",
                        a.name
                    )));
                }
            }
        }
        if a.name.contains('\0') || a.dir.contains('\0') {
            return Err(ModError::Invalid(
                "asset name/dir must not contain NUL".into(),
            ));
        }
        // only the single-file form has a name yet; folder-derived names
        // are de-duplicated by the post-expansion validate()
        if !a.name.trim().is_empty() && !asset_names.insert(a.name.clone()) {
            return Err(ModError::Invalid(format!(
                "two [[asset]] sections for name {:?} — a name is one row",
                a.name
            )));
        }
        validate_asset_host(a)?;
    }
    Ok(())
}

/// An `[[asset]]` `host`: `"auto"` (resolved at load time from the embedded
/// dir index) or a family-A opaque stream the loader can append to
/// (vp/tp/mcp/ep/cp) — never i.dat itself (that is the index).
fn validate_asset_host(a: &AssetSection) -> Result<(), ModError> {
    if a.host.eq_ignore_ascii_case(HOST_AUTO) {
        return Ok(());
    }
    let host_slug = !a.host.is_empty()
        && a.host
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !host_slug
        || !matches!(
            a9_formats::pack::classify(&a.host),
            Some((a9_formats::pack::Family::A, a9_formats::pack::Container::Opaque))
        )
    {
        return Err(ModError::Invalid(format!(
            "asset {:?}: host {:?} must be \"auto\" or a family-A pack the loader \
             can append to (vp/tp/mcp/ep/cp)",
            if a.name.is_empty() {
                a.folder.as_deref().unwrap_or("")
            } else {
                &a.name
            },
            a.host
        )));
    }
    Ok(())
}

/// A mod-relative path (folder form): not absolute, no drive, no `..`.
fn validate_mod_rel_path(p: &str, what: &str) -> Result<(), ModError> {
    if p.starts_with('/') || p.starts_with('\\') || p.contains(':') {
        return Err(ModError::Invalid(format!(
            "{what} {p:?} must be a relative path inside the mod dir"
        )));
    }
    if p.split(['/', '\\']).any(|c| c == "..") {
        return Err(ModError::Invalid(format!(
            "{what} {p:?} must not contain '..'"
        )));
    }
    Ok(())
}
