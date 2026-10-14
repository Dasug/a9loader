//! `a9ml mkpack` — build a pack file from raw payloads (fixtures/authoring).
//!
//! Four container kinds: family-B `images` (0x200 entries) and `csvs` (0x204
//! entries), a family-A opaque stream (`a`), and a family-A `i.dat` index
//! (`idat`). The result is byte-verified (decode + re-parse count) before it
//! is written.

use a9_formats as fmt;
use a9_formats::container::{parse_csvs, parse_images, CsvEntry, ImageEntry};
use a9_formats::pack::{self, Family};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;

pub fn run(container: &str, entries: &[String], out: &Path) -> Result<()> {
    let (file, family) = match container {
        "images" => build_images(entries)?,
        "csvs" => build_csvs(entries)?,
        "a" => build_opaque(entries)?,
        "idat" => build_idat(entries)?,
        other => bail!("--container must be images, csvs, a or idat, got {other:?}"),
    };
    // byte-verify the result before writing
    let decoded = pack::decode_pack(&file, family)?;
    let n = match container {
        "images" => parse_images(&decoded)?.len(),
        "csvs" => parse_csvs(&decoded)?.len(),
        "idat" => fmt::idat::parse(&decoded)?.entries.len(),
        _ => decoded.len(), // "a": raw decoded stream
    };
    if container != "a" {
        anyhow::ensure!(n == entries.len(), "round-trip count mismatch: {n} != {}", entries.len());
    }
    fs::write(out, &file).with_context(|| format!("writing {}", out.display()))?;
    println!(
        "{} written: {} {} (family {:?}), {:.1} MB decoded",
        out.display(),
        entries.len(),
        if container == "a" { "payload(s)" } else { "entries" },
        family,
        decoded.len() as f64 / (1024.0 * 1024.0)
    );
    Ok(())
}

/// `images`: `NAME=WIDTH:HEIGHT:FILE` → a family-B 0x200 container.
fn build_images(entries: &[String]) -> Result<(Vec<u8>, Family)> {
    let mut ents = Vec::new();
    for spec in entries {
        // NAME=WIDTH:HEIGHT:FILE (split from the right: the dims themselves
        // contain a colon)
        let (name, rest) = spec
            .split_once('=')
            .context("entry must be NAME=WIDTH:HEIGHT:FILE")?;
        let (dims, path) = rest
            .rsplit_once(':')
            .context("entry must be NAME=WIDTH:HEIGHT:FILE")?;
        let (w, h) = dims
            .split_once(':')
            .context("entry must be NAME=WIDTH:HEIGHT:FILE")?;
        let payload = fs::read(path).with_context(|| format!("reading {path}"))?;
        ents.push(ImageEntry {
            name: a9_formats::container::NameField::from_str(name)?,
            width: w.parse()?,
            height: h.parse()?,
            payload,
        });
    }
    Ok((
        pack::encode_pack_b(&a9_formats::container::serialize_images(&ents)),
        Family::B,
    ))
}

/// `csvs`: `NAME=FILE` → a family-B 0x204 container.
fn build_csvs(entries: &[String]) -> Result<(Vec<u8>, Family)> {
    let mut ents = Vec::new();
    for spec in entries {
        let (name, path) = spec
            .split_once('=')
            .context("entry must be NAME=FILE")?;
        let payload = fs::read(path).with_context(|| format!("reading {path}"))?;
        ents.push(CsvEntry {
            name: a9_formats::container::NameField::from_str(name)?,
            payload,
        });
    }
    Ok((
        pack::encode_pack_b(&a9_formats::container::serialize_csvs(&ents)),
        Family::B,
    ))
}

/// `a`: a family-A opaque stream — the named files' bytes, concatenated (the
/// M5 W1 fixture path — a host pack `V`).
fn build_opaque(entries: &[String]) -> Result<(Vec<u8>, Family)> {
    let mut decoded = Vec::new();
    for path in entries {
        decoded.extend_from_slice(&fs::read(path).with_context(|| format!("reading {path}"))?);
    }
    Ok((pack::encode_pack_a(&decoded), Family::A))
}

/// `idat`: a family-A `i.dat` index — `NAME=DIR:LEN:OFF` (OFF optional,
/// default 0); tails are synthesized clean (flag 0), pad = the Steam build's.
fn build_idat(entries: &[String]) -> Result<(Vec<u8>, Family)> {
    let mut rows = Vec::new();
    for spec in entries {
        let (name, rest) = spec.split_once('=').context("entry must be NAME=DIR:LEN:OFF")?;
        let (dir, rest) = rest.split_once(':').context("entry must be NAME=DIR:LEN:OFF")?;
        let (len_s, off_s) = rest
            .split_once(':')
            .unwrap_or((rest, "0"));
        let mut tail = [0u8; 9];
        tail[0..4].copy_from_slice(&len_s.parse::<u32>()?.to_le_bytes());
        tail[5..9].copy_from_slice(&off_s.parse::<u32>()?.to_le_bytes());
        rows.push(fmt::idat::Entry {
            name: name.to_string(),
            dir: dir.to_string(),
            tail,
        });
    }
    let idat = fmt::idat::Idat { pad: fmt::idat::PAD_STEAM, entries: rows };
    Ok((pack::encode_pack_a(&fmt::idat::serialize(&idat)), Family::A))
}
