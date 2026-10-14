//! `a9ml extract-csv` and `a9ml patch-csv` — the csvp authoring pair.
//!
//! `extract-csv` pulls one table out of the game's `data/csvp.bin` (verbatim
//! UTF-16, or a UTF-8 view). `patch-csv` applies `mode = "rows"` row-patch
//! files to a shipped table offline and reports the merge — the dry-run of
//! what the loader does at boot; nothing is installed.

use a9_formats as fmt;
use a9_formats::csv;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// The tables inside the game's `data/csvp.bin`: (name, payload bytes).
fn csvp_tables(game: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let path = game.join("data").join("csvp.bin");
    let file = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let decoded = fmt::pack::decode_pack(&file, fmt::pack::Family::B)
        .with_context(|| format!("decoding {}", path.display()))?;
    let entries = fmt::container::parse_csvs(&decoded)
        .with_context(|| format!("parsing the csvp container in {}", path.display()))?;
    Ok(entries
        .into_iter()
        .map(|e| (e.name.as_str(), e.payload))
        .collect())
}

/// A UTF-8 view of a UTF-16 table: canonical quoting, CRLF rows — readable and
/// editable in any editor. Not what the game stores, but a `mode = "rows"` row
/// patch may be UTF-8 (the loader re-encodes patched rows canonically anyway).
fn table_to_utf8(payload: &[u8]) -> Result<String> {
    let t = csv::Table::parse(payload)
        .context("the table is not a UTF-16 CSV (cannot convert to UTF-8)")?;
    let mut out = String::new();
    for r in &t.rows {
        let fields: Vec<String> = r
            .fields
            .iter()
            .map(|f| format!("\"{}\"", f.replace('"', "\"\"")))
            .collect();
        out.push_str(&fields.join(","));
        out.push_str("\r\n");
    }
    Ok(out)
}

pub fn extract(
    game: &Path,
    table: Option<&str>,
    out: Option<&Path>,
    list: bool,
    utf8: bool,
) -> Result<()> {
    let tables = csvp_tables(game)?;
    if list {
        for (name, payload) in &tables {
            println!("{name}\t{} B", payload.len());
        }
        return Ok(());
    }
    let name = table.context("--table is required (or use --list)")?;
    let (_, payload) = tables.iter().find(|(n, _)| n == name).with_context(|| {
        let names: Vec<&str> = tables.iter().map(|(n, _)| n.as_str()).collect();
        format!("table {name:?} is not in csvp (have: {})", names.join(", "))
    })?;
    let bytes = if utf8 {
        table_to_utf8(payload)?.into_bytes()
    } else {
        payload.clone()
    };
    match out {
        Some(p) => {
            fs::write(p, &bytes).with_context(|| format!("writing {}", p.display()))?;
            println!(
                "{} written: {name}, {} B{}",
                p.display(),
                bytes.len(),
                if utf8 { " (utf8 view)" } else { " (verbatim UTF-16)" }
            );
        }
        None => {
            use std::io::Write as _;
            std::io::stdout().write_all(&bytes)?;
        }
    }
    Ok(())
}

pub fn patch(
    game: &Path,
    table: &str,
    patches: &[PathBuf],
    out: Option<&Path>,
    utf8: bool,
) -> Result<()> {
    if patches.is_empty() {
        bail!("--patch is required at least once");
    }
    let tables = csvp_tables(game)?;
    let (_, payload) = tables.iter().find(|(n, _)| n == table).with_context(|| {
        let names: Vec<&str> = tables.iter().map(|(n, _)| n.as_str()).collect();
        format!("table {table:?} is not in csvp (have: {})", names.join(", "))
    })?;
    let base = csv::Table::parse(payload).context("the shipped table is not a UTF-16 CSV")?;
    let mut patch_tables = Vec::new();
    for p in patches {
        let bytes = fs::read(p).with_context(|| format!("reading {}", p.display()))?;
        let t = csv::parse_patch(&bytes).with_context(|| format!("parsing {}", p.display()))?;
        patch_tables.push(t);
    }
    let (merged, stats) = csv::merge_rows(&base, &patch_tables, csv::MergeMode::Rows)
        .context("merging the row patches")?;
    println!(
        "{table}: base {} rows -> merged {} rows ({} replaced, {} added, {} unchanged)",
        base.rows.len(),
        merged.rows.len(),
        stats.replaced,
        stats.appended,
        stats.unchanged
    );
    let keys: std::collections::BTreeSet<String> = patch_tables
        .iter()
        .flat_map(|t| t.patch_keys())
        .collect();
    for k in &keys {
        let present = merged.rows.iter().any(|r| r.key() == k);
        println!(
            "   row {k:?}: {}",
            if present { "present in the merged table" } else { "MISSING" }
        );
    }
    if let Some(out) = out {
        let bytes = if utf8 {
            table_to_utf8(&merged.serialize())?.into_bytes()
        } else {
            merged.serialize()
        };
        fs::write(out, &bytes).with_context(|| format!("writing {}", out.display()))?;
        println!("   written: {}", out.display());
    }
    Ok(())
}
