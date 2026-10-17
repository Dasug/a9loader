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
//!
//! [`run`] resolves the exe identity, then either [`check_book`] (the
//! update-day check) or [`emit_book`] (the generated text, written or printed).

use anyhow::{bail, Context, Result};
use std::path::Path;

use crate::util::civil_stamp;

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
            note: "the filter function (file 0x1a07b0) — train-list whitelist filter",
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
        // new RE work, then a mapping entry here
        "5.00.5634" => Ok(steam),
        _ => bail!("no core anchor set for build {id:?}"),
    }
}

pub(crate) fn rva_to_file(d: &[u8], rva: u32) -> Result<(usize, String)> {
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

pub fn run(exe: &Path, out: Option<&Path>, check: bool) -> Result<()> {
    let d = std::fs::read(exe).context("reading the exe")?;
    let build =
        a9_mod::buildid::from_exe_bytes(&d).context("this exe has no recognizable identity")?;
    let id = build.id.clone();
    let anchors = core_anchors(&id)?;

    // provenance hash: the format library's own md5 (proven byte-parity)
    let md5 = a9_formats::crypt::md5(&d);
    let hash: String = md5[..4].iter().map(|x| format!("{x:02x}")).collect();

    if check {
        let path = out.ok_or_else(|| anyhow::anyhow!("--check needs --out"))?;
        return check_book(path, &d, &id, &anchors, exe);
    }

    let full = emit_book(&d, &build, &id, &hash, &anchors, exe)?;
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

/// The full `builds.toml` text: the provenance header + the generated core
/// anchors.
fn emit_book(
    d: &[u8],
    build: &a9_mod::buildid::BuildId,
    id: &str,
    hash: &str,
    anchors: &[CoreAnchor],
    exe: &Path,
) -> Result<String> {
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
    for a in anchors {
        let (off, hex, where_note) = fingerprint(d, a)?;
        let _ = (off, where_note);
        body.push_str(&format!(
            "[build.{id:?}.anchors.{}]\nrva = {:#x}  # {}\nexpect = \"{hex}\"\n\n",
            a.name, a.rva, a.note
        ));
    }
    Ok(t + &body)
}

/// The update-day check: verify the generated CORE anchors against the book at
/// `path`. The book also carries hand-appended RE anchors the generator does
/// not know about, so a whole-file comparison always differs — only the core
/// anchors are compared (same rva, same expect bytes, case-insensitive).
fn check_book(path: &Path, d: &[u8], id: &str, anchors: &[CoreAnchor], exe: &Path) -> Result<()> {
    let cur = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;

    let field = |section: &str, key: &str| -> Option<String> {
        section.lines().find_map(|l| {
            let l = l.trim().strip_prefix(key)?.trim_start();
            let l = l.strip_prefix('=')?.trim();
            let v = l.split('#').next().unwrap_or(l).trim();
            Some(v.trim_matches('"').to_string())
        })
    };
    let mut problems: Vec<String> = Vec::new();
    let mut seen = 0usize;
    for a in anchors {
        let (_, hex, _) = fingerprint(d, a)?;
        let want = hex.to_ascii_lowercase().replace(' ', "");
        let header = format!("[build.{id:?}.anchors.{}]", a.name);
        let Some(pos) = cur.find(&header) else {
            problems.push(format!("{}: section missing", a.name));
            continue;
        };
        seen += 1;
        let section = &cur[pos + header.len()..];
        let section = section.split("\n[").next().unwrap_or(section);
        let got_rva = field(section, "rva")
            .and_then(|v| u64::from_str_radix(v.trim_start_matches("0x").trim_start_matches("0X"), 16).ok());
        let got_expect = field(section, "expect")
            .map(|v| v.to_ascii_lowercase().replace(' ', ""));
        if got_rva != Some(a.rva as u64) {
            problems.push(format!(
                "{}: rva mismatch (book {:?}, generated {:#x})",
                a.name,
                field(section, "rva").unwrap_or_default(),
                a.rva
            ));
        } else if got_expect.as_deref() != Some(want.as_str()) {
            problems.push(format!("{}: expect bytes differ from the exe", a.name));
        }
    }
    if !problems.is_empty() {
        for p in &problems {
            println!("MISMATCH: {p}");
        }
        bail!(
            "{}: {} of {} core anchor(s) do not match {}",
            path.display(),
            problems.len(),
            anchors.len(),
            exe.display()
        );
    }
    let total = cur.matches(&format!("[build.{id:?}.anchors.")).count();
    println!(
        "check ok: {} core anchor(s) verified against {}; {} hand-appended anchor(s) retained (not generated)",
        seen,
        exe.display(),
        total.saturating_sub(seen)
    );
    Ok(())
}
