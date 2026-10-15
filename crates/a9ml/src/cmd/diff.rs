//! `a9ml diff-pack` and `a9ml diff-csv` — turn a base/repacked pair into a
//! ready-to-ship mod dir (mod.toml + payload files).
//!
//! `diff-pack` compares two family-B image packs; `diff-csv` compares two
//! csvp containers (and, with `--rows`, ships only the changed/added rows as a
//! `mode = "rows"` patch). Both refuse a removed entry/table loudly: removal
//! is not expressible in a mod manifest.

use a9_formats::container::{parse_csvs, parse_images, CsvEntry, ImageEntry};
use a9_formats::csv;
use a9_formats::pack::{self, Family};
use a9_mod::manifest::{CsvMode, CsvSection, Manifest, PackEntry, PackSection};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

/// A filesystem-safe payload file name (names come from the pack, so keep
/// only the characters we want in a path).
fn sanitized(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect()
}

fn manifest_toml(m: &Manifest) -> Result<String> {
    toml::to_string(m).context("serializing mod.toml")
}

/// Decode + parse a family-B image pack (`which` names it in the error).
fn read_images(path: &Path, which: &str) -> Result<Vec<ImageEntry>> {
    let f = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    parse_images(&pack::decode_pack(&f, Family::B)?)
        .with_context(|| format!("parsing the {which} pack (family-B 0x200 expected)"))
}

/// Decode + parse a family-B csvp container (`which` names it in the error).
fn read_csvs(path: &Path, which: &str) -> Result<Vec<CsvEntry>> {
    let f = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    parse_csvs(&pack::decode_pack(&f, Family::B)?)
        .with_context(|| format!("parsing the {which} csvp (0x204 container expected)"))
}

/// Write one changed entry's payload and build its `[[pack.entry]]` row.
/// Returns the row and the payload size (for the summary).
fn write_entry_payload(out: &Path, e: &ImageEntry) -> Result<(PackEntry, usize)> {
    let n = e.name.as_str();
    let file = format!("entries/{}", sanitized(&n));
    fs::write(out.join(&file), &e.payload)
        .with_context(|| format!("writing {}", out.join(&file).display()))?;
    Ok((
        PackEntry {
            name: n.clone(),
            width: e.width,
            height: e.height,
            file,
        },
        e.payload.len(),
    ))
}

/// Write `mod.toml` for a generated mod (empty `[[asset]]`/`[[loose_asset]]` etc.).
fn write_mod(
    out: &Path,
    id: &str,
    name: &str,
    author: &str,
    game_builds: &[String],
    pack: Vec<PackSection>,
    csv: Vec<CsvSection>,
) -> Result<()> {
    let m = Manifest {
        id: id.to_string(),
        name: name.to_string(),
        version: semver::Version::new(1, 0, 0),
        author: author.to_string(),
        description: None,
        license: None,
        url: None,
        game_builds: game_builds.to_vec(),
        priority: a9_mod::manifest::DEFAULT_PRIORITY,
        supplies: Vec::new(),
        requires: Vec::new(),
        conflicts: Vec::new(),
        patch: Vec::new(),
        hook: Vec::new(),
        pack,
        csv,
        train_csv: Default::default(),
        asset: Vec::new(),
        loose_asset: Vec::new(),
        train: Vec::new(),
    };
    m.validate().map_err(|e| anyhow::anyhow!("generated manifest invalid: {e}"))?;
    fs::create_dir_all(out).context("creating the mod dir")?;
    fs::write(out.join("mod.toml"), manifest_toml(&m)?).context("writing mod.toml")?;
    Ok(())
}

pub fn diff_pack(
    pack: &str,
    base: &Path,
    modded: &Path,
    out: &Path,
    id: &str,
    name: &str,
    author: &str,
    game_builds: &[String],
) -> Result<()> {
    let base_e = read_images(base, "base")?;
    let mod_e = read_images(modded, "modded")?;

    let base_map: BTreeMap<String, &ImageEntry> =
        base_e.iter().map(|e| (e.name.as_str(), e)).collect();
    let mod_names: BTreeSet<String> =
        mod_e.iter().map(|e| e.name.as_str()).collect();

    let removed: Vec<String> = base_map
        .keys()
        .filter(|n| !mod_names.contains(*n))
        .cloned()
        .collect();
    if !removed.is_empty() {
        bail!(
            "{} entries exist in the base but not in the modded pack — removal is not \
             expressible as a [[pack]] mod: {}{}",
            removed.len(),
            removed.iter().take(5).cloned().collect::<Vec<_>>().join(", "),
            if removed.len() > 5 { ", …" } else { "" }
        );
    }

    let entries_dir = out.join("entries");
    fs::create_dir_all(&entries_dir).context("creating entries/")?;
    let mut section = PackSection {
        pack: pack.to_string(),
        entry: Vec::new(),
    };
    let mut added = 0usize;
    let mut replaced = 0usize;
    let mut total_bytes = 0usize;
    for e in &mod_e {
        let existing = base_map.get(&e.name.as_str());
        let changed = match existing {
            Some(b) => b.payload != e.payload || b.width != e.width || b.height != e.height,
            None => true,
        };
        if !changed {
            continue;
        }
        if existing.is_some() {
            replaced += 1;
        } else {
            added += 1;
        }
        let (entry, bytes) = write_entry_payload(out, e)?;
        total_bytes += bytes;
        section.entry.push(entry);
    }
    if section.entry.is_empty() {
        bail!("no differences found between the packs — nothing to generate");
    }
    let n_entries = section.entry.len();
    write_mod(
        out,
        id,
        name,
        author,
        game_builds,
        vec![section],
        Vec::new(),
    )?;
    println!(
        "mod '{id}' generated: pack {pack} — {added} added, {replaced} replaced, {n_entries} entries, {:.1} MB payloads -> {}",
        total_bytes as f64 / (1024.0 * 1024.0),
        out.display()
    );
    Ok(())
}

pub fn diff_csv(
    base: &Path,
    modded: &Path,
    out: &Path,
    id: &str,
    name: &str,
    author: &str,
    game_builds: &[String],
    rows_mode: bool,
) -> Result<()> {
    let base_e = read_csvs(base, "base")?;
    let mod_e = read_csvs(modded, "modded")?;
    let base_map: BTreeMap<String, &CsvEntry> =
        base_e.iter().map(|e| (e.name.as_str(), e)).collect();
    let mod_names: BTreeSet<String> =
        mod_e.iter().map(|e| e.name.as_str()).collect();
    let removed: Vec<String> = base_map
        .keys()
        .filter(|n| !mod_names.contains(*n))
        .cloned()
        .collect();
    if !removed.is_empty() {
        bail!(
            "{} tables exist in the base but not in the modded csvp — removal is not \
             expressible: {}",
            removed.len(),
            removed.join(", ")
        );
    }
    let csv_dir = out.join("csv");
    fs::create_dir_all(&csv_dir).context("creating csv/")?;
    let mut sections = Vec::new();
    let mut added = 0usize;
    let mut replaced = 0usize;
    let mut row_replaced_total = 0usize;
    let mut row_added_total = 0usize;
    for e in &mod_e {
        let existing = base_map.get(&e.name.as_str());
        let changed = existing.map_or(true, |b| b.payload != e.payload);
        if !changed {
            continue;
        }
        if existing.is_some() {
            replaced += 1;
        } else {
            added += 1;
        }
        let (section, r_replaced, r_added) = diff_one_table(out, e, existing.copied(), rows_mode)?;
        sections.push(section);
        row_replaced_total += r_replaced;
        row_added_total += r_added;
    }
    if sections.is_empty() {
        bail!("no differences found between the csvp files — nothing to generate");
    }
    let n_tables = sections.len();
    let n_rows = sections.iter().filter(|c| c.mode == CsvMode::Rows).count();
    write_mod(out, id, name, author, game_builds, Vec::new(), sections)?;
    println!(
        "mod '{id}' generated: csvp — {added} added, {replaced} replaced, {n_tables} tables \
         ({n_rows} as row patches; {row_replaced_total} rows replaced, {row_added_total} rows added) -> {}",
        out.display()
    );
    Ok(())
}

/// Diff one changed table: write its payload (whole table, or a compact row
/// patch under `--rows`), print the per-table line, and return the `[[csv]]`
/// section plus its (replaced, added) row counts.
fn diff_one_table(
    out: &Path,
    e: &CsvEntry,
    existing: Option<&CsvEntry>,
    rows_mode: bool,
) -> Result<(CsvSection, usize, usize)> {
    let n = e.name.as_str();
    // row-level view of the change (best effort — reports nothing when a side
    // is not a parseable UTF-16 table)
    let (r_replaced, r_added, r_removed) = match existing {
        Some(b) => diff_rows(&b.payload, &e.payload),
        None => (Vec::new(), Vec::new(), Vec::new()),
    };
    let file = format!("csv/{}", sanitized(&n));
    // A pure row-level change (existing table, parseable, nothing removed) can
    // ship as a compact row patch under --rows.
    let as_rows = rows_mode
        && existing.is_some()
        && r_removed.is_empty()
        && !(r_replaced.is_empty() && r_added.is_empty());
    let section = if as_rows {
        let modded_tbl = csv::Table::parse(&e.payload)
            .with_context(|| format!("parsing changed table {n} (row mode)"))?;
        let keys: BTreeSet<String> = r_replaced
            .iter()
            .chain(r_added.iter())
            .cloned()
            .collect();
        let patch = row_patch_bytes(&modded_tbl, &keys);
        fs::write(out.join(&file), &patch)
            .with_context(|| format!("writing {}", out.join(&file).display()))?;
        println!(
            "    {n}: row patch — {} row(s) replaced, {} added{}",
            r_replaced.len(),
            r_added.len(),
            list_keys(&r_replaced, &r_added)
        );
        CsvSection {
            name: n.to_string(),
            file,
            mode: CsvMode::Rows,
            row_keys: Vec::new(),
        }
    } else {
        fs::write(out.join(&file), &e.payload)
            .with_context(|| format!("writing {}", out.join(&file).display()))?;
        let row_note = if r_removed.is_empty() {
            format!(
                "{} row(s) replaced, {} added",
                r_replaced.len(),
                r_added.len()
            )
        } else {
            format!(
                "{} row(s) replaced, {} added, {} removed — shipped as a whole table",
                r_replaced.len(),
                r_added.len(),
                r_removed.len()
            )
        };
        println!(
            "    {n}: whole table — {row_note}{}",
            list_keys(&r_replaced, &r_added)
        );
        CsvSection {
            name: n.to_string(),
            file,
            mode: CsvMode::Replace,
            row_keys: Vec::new(),
        }
    };
    Ok((section, r_replaced.len(), r_added.len()))
}

/// Row-level comparison of two table payloads: (replaced keys, added keys,
/// removed keys), each sorted. Empty when a side is not a parseable table or
/// has no keyed rows.
fn diff_rows(base: &[u8], modded: &[u8]) -> (Vec<String>, Vec<String>, Vec<String>) {
    let (Ok(b), Ok(m)) = (csv::Table::parse(base), csv::Table::parse(modded)) else {
        return (Vec::new(), Vec::new(), Vec::new());
    };
    let bmap: BTreeMap<&str, &csv::Row> = b
        .rows
        .iter()
        .filter(|r| !r.key().is_empty())
        .map(|r| (r.key(), r))
        .collect();
    let mmap: BTreeMap<&str, &csv::Row> = m
        .rows
        .iter()
        .filter(|r| !r.key().is_empty())
        .map(|r| (r.key(), r))
        .collect();
    let mut replaced = Vec::new();
    let mut added = Vec::new();
    for (k, mr) in &mmap {
        match bmap.get(k) {
            Some(br) => {
                if br.fields != mr.fields {
                    replaced.push((*k).to_string());
                }
            }
            None => added.push((*k).to_string()),
        }
    }
    let removed = bmap
        .keys()
        .filter(|k| !mmap.contains_key(**k))
        .map(|k| (*k).to_string())
        .collect();
    (replaced, added, removed)
}

/// Serialize just the named keys' rows from `modded` as a UTF-16 row patch.
fn row_patch_bytes(modded: &csv::Table, keys: &BTreeSet<String>) -> Vec<u8> {
    let mut t = csv::Table::new();
    for r in &modded.rows {
        if !r.key().is_empty() && keys.contains(r.key()) {
            t.push_row(r.fields.clone());
        }
    }
    t.serialize()
}

/// A short ", e.g. R1, R2" tail naming a few changed keys.
fn list_keys(replaced: &[String], added: &[String]) -> String {
    let mut names: Vec<&str> = replaced.iter().map(String::as_str).take(4).collect();
    names.extend(added.iter().map(String::as_str).take(4));
    if names.is_empty() {
        return String::new();
    }
    let shown = names.len();
    let total = replaced.len() + added.len();
    let tail = if total > shown {
        format!(", … (+{})", total - shown)
    } else {
        String::new()
    };
    format!(" (e.g. {}{tail})", names.join(", "))
}
