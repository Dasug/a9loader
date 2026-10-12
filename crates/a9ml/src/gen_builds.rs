//! `a9ml gen-builds` — regenerate the anchor set of the build address book
//! from a live exe (the SKSE address-library model; research lesson 7).
//!
//! The tool knows the CORE anchor set per build id (what each anchor is and
//! how to fingerprint it); everything it emits carries provenance (source exe,
//! file hash, date). RE-derived anchors beyond this core set are appended to
//! builds.toml by hand with their own provenance — they are RE findings, not
//! derivable data.
//!
//! Usage:
//!   a9ml gen-builds --exe ATrain9v5.exe                     # print the book
//!   a9ml gen-builds --exe ATrain9v5.exe --out builds.toml   # write it
//!   a9ml gen-builds --exe ATrain9v5.exe --out builds.toml --check
//!     # verify the existing book matches the exe exactly (update-day check)

use anyhow::{bail, Context, Result};
use std::path::Path;

/// the anchor core set, keyed by build id.
struct CoreAnchor {
    rva: u32,
    name: &'static str,
    note: &'static str,
    /// fingerprint length in bytes
    len: usize,
}

fn core_anchors(id: &str) -> Result<Vec<CoreAnchor>> {
    let steam = Vec::from([
        CoreAnchor {
            rva: 0x1A13B0,
            name: "train_list_filter",
            note: "FUN_1401a13b0 (file 0x1a07b0) — train-list whitelist filter",
            len: 16,
        },
        CoreAnchor {
            rva: 0x6FC1C0,
            name: "train_list_whitelist",
            note: "97-entry u32 table (file 0x6fb5c0); expect = first entry",
            len: 4,
        },
        CoreAnchor {
            rva: 0x41D40C,
            name: "cfile_open",
            note: "MFC CFile::Open — the ONE file-open funnel (file 0x41c80c)",
            len: 16,
        },
        CoreAnchor {
            rva: 0x6FC430,
            name: "form_list",
            note: "UTF-16 comma-separated FORM names (file 0x6fb830); head fingerprint",
            len: 16,
        },
    ]);
    match id {
        // the rvas are build-specific; extending support to another build =
        // RE work recorded in the notes, then a mapping entry here
        "5.00.5634" => Ok(steam),
        _ => bail!("no core anchor set for build {id:?}"),
    }
}

fn rva_to_file(d: &[u8], rva: u32) -> Result<(usize, String)> {
    let pe = u32::from_le_bytes(d[0x3c..0x40].try_into().unwrap()) as usize;
    let nsec = u16::from_le_bytes(d[pe + 6..pe + 8].try_into().unwrap());
    let opt = u16::from_le_bytes(d[pe + 20..pe + 22].try_into().unwrap());
    let secs = pe + 24 + opt as usize;
    for i in 0..nsec as usize {
        let sh = secs + i * 40;
        let vsz = u32::from_le_bytes(d[sh + 8..sh + 12].try_into().unwrap());
        let va = u32::from_le_bytes(d[sh + 12..sh + 16].try_into().unwrap());
        let rsz = u32::from_le_bytes(d[sh + 16..sh + 20].try_into().unwrap());
        let raw = u32::from_le_bytes(d[sh + 20..sh + 24].try_into().unwrap());
        if va <= rva && rva < va + vsz.max(rsz) {
            let name: String = String::from_utf8_lossy(&d[sh..sh + 8])
                .trim_end_matches('\0')
                .to_string();
            return Ok(((rva - va + raw) as usize, name));
        }
    }
    bail!("rva {rva:#x} not in any mapped section")
}

fn fingerprint(d: &[u8], a: &CoreAnchor) -> Result<(usize, String, String)> {
    let (off, sec) = rva_to_file(d, a.rva).with_context(|| format!("anchor {}", a.name))?;
    let bytes = d
        .get(off..off + a.len)
        .filter(|b| b.len() == a.len)
        .ok_or_else(|| anyhow::anyhow!("anchor {} bytes out of range", a.name))?;
    let hex = bytes
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join(",");
    Ok((off, hex, format!("file {off:#x} in {sec}")))
}

fn civil_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let sod = (secs % 86_400) as u32;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

pub fn run(exe: &Path, out: Option<&Path>, check: bool) -> Result<()> {
    let d = std::fs::read(exe).context("reading the exe")?;
    let build =
        a9_mod::buildid::from_exe_bytes(&d).context("this exe has no recognizable identity")?;
    let id = build.id.clone();
    let anchors = core_anchors(&id)?;

    // provenance hash: the format library's own md5 (proven byte-parity in M1)
    let md5 = a9_formats::crypt::md5(&d);
    let hash: String = md5[..4].iter().map(|x| format!("{x:02x}")).collect();

    let mut t = String::new();
    t.push_str("# a9loader build address book — one entry per supported game build.\n");
    t.push_str("# Anchors are named; mod manifests reference anchor names, never raw RVAs\n");
    t.push_str("# (a raw rva target is the diagnostic escape hatch and must carry its own\n");
    t.push_str("# `expect` bytes). `expect` values are the ORIGINAL bytes at the anchor,\n");
    t.push_str("# verified in the mapped image before any write — a game update that shifts\n");
    t.push_str("# code fails loudly instead of corrupting memory. These per-anchor checks\n");
    t.push_str("# are the in-image build fingerprint; the build id itself comes from the\n");
    t.push_str("# exe file's VERSIONINFO (parsed from bytes, never via the version.dll APIs\n");
    t.push_str("# our proxy forwards).\n#\n");
    t.push_str(&format!(
        "# Provenance: generated by `a9ml gen-builds` from {},\n",
        exe.display()
    ));
    t.push_str(&format!(
        "# build id {id} ({}), exe md5[:8] {hash}, generated {} UTC\n",
        build.tuple(),
        civil_stamp()
    ));
    t.push_str("# RE-derived anchors beyond this set are manually appended to this file.\n\n");

    let mut body = String::new();
    for a in &anchors {
        let (off, hex, where_note) = fingerprint(&d, a)?;
        let _ = (off, where_note);
        body.push_str(&format!(
            "[build.{id:?}.anchors.{}]\nrva = {:#x}  # {}\nexpect = \"{hex}\"\n\n",
            a.name, a.rva, a.note
        ));
    }
    let full = t + &body;

    if check {
        let path = out.ok_or_else(|| anyhow::anyhow!("--check needs --out"))?;
        let cur = std::fs::read_to_string(path).unwrap_or_default();
        let norm = |s: &str| -> String {
            s.lines()
                .filter(|l| {
                    let l = l.trim_start();
                    !l.starts_with('#') && !l.is_empty()
                })
                .map(|l| l.split('#').next().unwrap_or(l).trim().to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };
        if norm(&cur) != norm(&full) {
            // the file also carries hand-appended RE anchors, so a mismatch is
            // expected when those exist — say so instead of failing the check
            println!(
                "NOTE: {} differs from the generated core (hand-appended RE anchors are kept; compare by section)",
                path.display()
            );
            bail!(
                "{} does not match the generated core exactly — inspect the diff manually",
                path.display()
            );
        }
        println!("check ok: {} matches the exe's core anchors", path.display());
        return Ok(());
    }

    match out {
        Some(path) => {
            std::fs::write(path, &full)?;
            println!(
                "wrote {}: build {id} with {} core anchors",
                path.display(),
                anchors.len()
            );
        }
        None => print!("{full}"),
    }
    Ok(())
}
