//! `a9ml gen-dirindex` — regenerate the per-build `dir -> pack` index from a
//! live install (M5 W1b).
//!
//! The host pack of a `host = "auto"` asset is not a free choice: every
//! `pack_master_map_query` consumer addresses the payload as
//! `workspace + off`, so the host must be the pack that is resident while the
//! asset's consumers run. The shipped i.dat rows encode exactly that timing —
//! a row's `(off, len)` selects bytes in whichever pack was current — so the
//! generator recovers the mapping empirically: for every shipped row, decode
//! each pack's buffer and test whether it holds a plausible payload at
//! `(off, len)` (the historical corpus `row_match.py` method), then aggregate
//! per `dir`.
//!
//! A row's content kind drives the test: `.x` records must carry the `ADX0`
//! magic with a matching chunk length (a strong, low-false-positive signal);
//! textures must carry a known image magic; the weaker text-like kinds
//! (`.att`/`.dir`/`.csv`/other) fall back to shape heuristics. A `dir` is
//! resolved when one pack is the unique owner — every kind-strong row of the
//! dir lives in it (or, when no strong rows exist, every row does) — and is
//! recorded **ambiguous** when its rows span packs. Anything that verifies no
//! pack is left out entirely (unknown).
//!
//! Usage:
//!   a9ml gen-dirindex --data-dir <game data> --out crates/a9-mod/dirindex.toml
//!   a9ml gen-dirindex --data-dir <game data> --out <file> --check   # update-day
//!   a9ml gen-dirindex --data-dir <game data>                        # report only
//!
//! The build id comes from the game exe (searched next to the data dir); pass
//! `--exe` to point at it explicitly. Decoded packs are held one at a time.
//!
//! [`generate`] is a sequence of named steps: [`build_identity`] →
//! [`read_rows`] → [`pack_keys`] → [`match_rows`] → [`aggregate`] →
//! [`rescue_names`] → [`summarize`]. [`run`] renders the table
//! ([`emit_table`]), the report ([`report_text`]) and the write/check
//! ([`write_or_check`]).

use a9_formats as fmt;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::util::{ascii_like, civil_stamp};

/// A shipped row's content family, from its name suffix (the discriminator).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    X,
    Att,
    Dir,
    Tex,
    Csv,
    Other,
}

impl Kind {
    fn of(name: &str) -> Kind {
        let n = name.to_ascii_lowercase();
        if n.ends_with(".x") {
            Kind::X
        } else if n.ends_with(".att") {
            Kind::Att
        } else if n.ends_with(".dir") {
            Kind::Dir
        } else if n.ends_with(".dds")
            || n.ends_with(".bmp")
            || n.ends_with(".tga")
            || n.ends_with(".png")
            || n.ends_with(".jpg")
        {
            Kind::Tex
        } else if n.ends_with(".csv") {
            Kind::Csv
        } else {
            Kind::Other
        }
    }

    /// Kinds whose magic check is strong enough to pin a row to one pack. The
    /// weaker kinds are only used when a dir has no strong rows at all.
    fn strong(self) -> bool {
        matches!(self, Kind::X | Kind::Tex)
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Kind::X => "x",
            Kind::Att => "att",
            Kind::Dir => "dir",
            Kind::Tex => "tex",
            Kind::Csv => "csv",
            Kind::Other => "other",
        })
    }
}

/// Does `buf` hold a payload of `kind` at `(off, len)`? The one place the
/// heuristics live — pure, unit-tested on synthetic buffers.
fn row_matches(kind: Kind, off: u32, len: u32, buf: &[u8]) -> bool {
    let (off, len) = (off as usize, len as usize);
    let Some(end) = off.checked_add(len) else {
        return false;
    };
    let Some(blob) = buf.get(off..end) else {
        return false;
    };
    match kind {
        Kind::X => {
            // `ADX0` chunk header + the 24-bit payload length at +8
            len >= 12
                && &blob[..4] == b"ADX0"
                && (u32::from_le_bytes(blob[8..12].try_into().unwrap()) & 0x00ff_ffff)
                    == (len as u32).wrapping_sub(12)
        }
        Kind::Tex => {
            blob.len() >= 4
                && (blob.starts_with(b"DDS ")
                    || blob.starts_with(b"BM  ")
                    || blob.starts_with(b"\x89PNG")
                    || blob.starts_with(b"BM")
                    || blob.starts_with(b"DD"))
        }
        Kind::Att | Kind::Csv | Kind::Other => ascii_like(blob),
        Kind::Dir => utf16_name_run(blob),
    }
}

fn utf16_name_run(b: &[u8]) -> bool {
    if b.len() < 4 {
        return false;
    }
    let mut ok = 0usize;
    let mut total = 0usize;
    let mut i = 0;
    while i + 1 < b.len().min(64) {
        total += 1;
        if b[i] == 0 || (0x20..0x7f).contains(&b[i]) && b[i + 1] == 0 {
            ok += 1;
        }
        i += 2;
    }
    total > 0 && ok * 10 >= total * 6
}

/// A discovered shipped row (name/dir + the i.dat tail fields).
struct Row {
    name: String,
    dir: String,
    off: u32,
    len: u32,
    kind: Kind,
}

/// How one `dir`'s rows resolve to a host pack.
enum DirResolution {
    Host(String),
    /// packs the dir's (strong, or all) rows span
    Ambiguous(BTreeSet<String>),
    /// no pack verified any row
    Unverified,
}

/// Aggregate one dir's per-row pack hits into a resolution. `hits[i]` are the
/// packs that verified row `i` (parallel to `kinds`).
fn resolve_dir(hits: &[BTreeSet<String>], kinds: &[Kind]) -> DirResolution {
    let strong: Vec<usize> = (0..kinds.len()).filter(|&i| kinds[i].strong()).collect();
    let use_idx = if strong.is_empty() {
        (0..kinds.len()).collect::<Vec<_>>()
    } else {
        strong
    };
    let n = use_idx.len();
    let mut cov: BTreeMap<&str, usize> = BTreeMap::new();
    for &i in &use_idx {
        for p in &hits[i] {
            *cov.entry(p.as_str()).or_default() += 1;
        }
    }
    match cov.len() {
        0 => DirResolution::Unverified,
        1 => DirResolution::Host(cov.keys().next().unwrap().to_string()),
        _ => {
            let full: Vec<&str> = cov
                .iter()
                .filter(|(_, c)| **c == n)
                .map(|(p, _)| *p)
                .collect();
            if full.len() == 1 {
                DirResolution::Host(full[0].to_string())
            } else {
                DirResolution::Ambiguous(cov.keys().map(|p| p.to_string()).collect())
            }
        }
    }
}

fn find_exe(data_dir: &Path, explicit: Option<&Path>) -> Option<(PathBuf, Vec<u8>)> {
    if let Some(p) = explicit {
        return std::fs::read(p).ok().map(|b| (p.to_path_buf(), b));
    }
    for dir in [data_dir, data_dir.parent().unwrap_or(data_dir)] {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut c: Vec<PathBuf> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension().map(|x| x.eq_ignore_ascii_case("exe")).unwrap_or(false)
                    && p.file_name()
                        .map(|n| n.to_string_lossy().starts_with("ATrain9"))
                        .unwrap_or(false)
            })
            .collect();
        c.sort();
        if let Some(p) = c.into_iter().next() {
            if let Ok(b) = std::fs::read(&p) {
                return Some((p, b));
            }
        }
    }
    None
}

/// What [`generate`] learned, for the report and tests.
pub struct Report {
    pub packs: usize,
    pub decode_failed: Vec<String>,
    pub rows: usize,
    pub matched: usize,
    pub kinds: BTreeMap<String, usize>,
    pub dirs: usize,
    pub resolved: usize,
    pub unverified: usize,
    pub ambiguous: BTreeMap<String, Vec<String>>,
    pub train_dirs: usize,
    pub train_ambiguous: usize,
}

/// The per-dir aggregation of one generation run.
struct Aggregated {
    /// number of distinct shipped dirs
    dir_count: usize,
    dirs: BTreeMap<String, String>,
    ambiguous: BTreeMap<String, Vec<String>>,
    unverified: Vec<String>,
    train_dirs: usize,
    train_ambiguous: usize,
}

/// The generator core: decode the shipped index + every pack, aggregate the
/// per-dir resolution, and return the finished table. Pure IO in, table out.
pub fn generate(
    data_dir: &Path,
    exe: Option<&Path>,
) -> Result<(a9_mod::buildid::BuildId, PathBuf, a9_mod::dirindex::DirIndex, Report)> {
    if !data_dir.is_dir() {
        bail!("not a directory: {}", data_dir.display());
    }
    // ---- build identity (the table is per build)
    let (build, exe_path) = build_identity(data_dir, exe)?;
    let id = build.id.clone();

    // ---- the shipped index + the packs
    let rows = read_rows(data_dir)?;
    let pack_names = pack_keys(data_dir)?;
    let (hits, decoded_packs, decode_failed) = match_rows(data_dir, &rows, &pack_names);

    // ---- aggregate per dir, then rescue names, then summarize
    let agg = aggregate(&rows, &hits);
    let names = rescue_names(&rows, &hits, &agg.ambiguous);
    let report = summarize(&rows, &hits, &agg, decoded_packs, decode_failed);

    let mut index = a9_mod::dirindex::DirIndex::default();
    index.build.insert(
        id,
        a9_mod::dirindex::BuildIndex {
            names,
            dirs: agg.dirs,
            ambiguous: agg.ambiguous,
        },
    );
    Ok((build, exe_path, index, report))
}

/// The exe + its build id (the dir index is keyed by build id).
fn build_identity(
    data_dir: &Path,
    exe: Option<&Path>,
) -> Result<(a9_mod::buildid::BuildId, PathBuf)> {
    let (exe_path, exe_bytes) = find_exe(data_dir, exe).ok_or_else(|| {
        anyhow::anyhow!(
            "cannot find an ATrain9*.exe next to {} — pass --exe (the dir index is per build)",
            data_dir.display()
        )
    })?;
    let build = a9_mod::buildid::from_exe_bytes(&exe_bytes)
        .ok_or_else(|| anyhow::anyhow!("{} has no VERSIONINFO", exe_path.display()))?;
    Ok((build, exe_path))
}

/// Parse the shipped i.dat into the rows to test.
fn read_rows(data_dir: &Path) -> Result<Vec<Row>> {
    let idat_path = data_dir.join("i.dat");
    let idat_file = std::fs::read(&idat_path)
        .with_context(|| format!("reading {}", idat_path.display()))?;
    let decoded = fmt::pack::decode_pack(&idat_file, fmt::pack::Family::A)
        .context("decoding the shipped i.dat")?;
    let idat = fmt::idat::parse(&decoded).context("parsing the shipped i.dat")?;
    Ok(idat
        .entries
        .iter()
        .map(|e| Row {
            name: e.name.clone(),
            dir: e.dir.clone(),
            off: e.v3(),
            len: e.v1(),
            kind: Kind::of(&e.name),
        })
        .collect())
}

/// The pack keys to decode (every recognized pack in the data dir, i.dat
/// excluded).
fn pack_keys(data_dir: &Path) -> Result<Vec<String>> {
    let rd = std::fs::read_dir(data_dir)
        .with_context(|| format!("reading {}", data_dir.display()))?;
    let mut files: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    let mut pack_names = Vec::new();
    for p in files {
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        if fmt::pack::classify(&name).is_none() || name.eq_ignore_ascii_case("i.dat") {
            continue;
        }
        pack_names.push(fmt::pack::classify_key(&name));
    }
    Ok(pack_names)
}

/// Decode each pack (one at a time) and accumulate, per row, the packs whose
/// buffer verifies it. Returns the per-row hits, the decoded count and the
/// decode failures.
fn match_rows(
    data_dir: &Path,
    rows: &[Row],
    pack_names: &[String],
) -> (Vec<BTreeSet<String>>, usize, Vec<String>) {
    let mut hits: Vec<BTreeSet<String>> = vec![BTreeSet::new(); rows.len()];
    let mut decoded_packs = 0usize;
    let mut decode_failed: Vec<String> = Vec::new();
    for key in pack_names {
        let bin = data_dir.join(format!("{key}.bin"));
        let path = if bin.is_file() { bin } else { data_dir.join(key) };
        let Ok(file) = std::fs::read(&path) else {
            decode_failed.push(key.clone());
            continue;
        };
        let Some((family, _)) = fmt::pack::classify(key) else {
            continue;
        };
        match fmt::pack::decode_pack(&file, family) {
            Ok(buf) => {
                decoded_packs += 1;
                for (i, r) in rows.iter().enumerate() {
                    if row_matches(r.kind, r.off, r.len, &buf) {
                        hits[i].insert(key.clone());
                    }
                }
            }
            Err(e) => decode_failed.push(format!("{key} ({e})")),
        }
    }
    (hits, decoded_packs, decode_failed)
}

/// Aggregate the per-row hits into one resolution per dir.
fn aggregate(rows: &[Row], hits: &[BTreeSet<String>]) -> Aggregated {
    let mut dir_rows: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, r) in rows.iter().enumerate() {
        dir_rows.entry(r.dir.as_str()).or_default().push(i);
    }
    let mut dirs: BTreeMap<String, String> = BTreeMap::new();
    let mut ambiguous: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut unverified: Vec<String> = Vec::new();
    for (dir, idxs) in &dir_rows {
        let h: Vec<BTreeSet<String>> = idxs.iter().map(|&i| hits[i].clone()).collect();
        let k: Vec<Kind> = idxs.iter().map(|&i| rows[i].kind).collect();
        match resolve_dir(&h, &k) {
            DirResolution::Host(p) => {
                dirs.insert(dir.to_string(), p);
            }
            DirResolution::Ambiguous(ps) => {
                ambiguous.insert(dir.to_string(), ps.into_iter().collect());
            }
            DirResolution::Unverified => unverified.push(dir.to_string()),
        }
    }
    let is_train = |d: &str| d == "train" || d.starts_with("train\\");
    let train_dirs = dir_rows.keys().filter(|d| is_train(d)).count();
    let train_ambiguous = ambiguous.keys().filter(|d| is_train(d)).count();
    Aggregated {
        dir_count: dir_rows.len(),
        dirs,
        ambiguous,
        unverified,
        train_dirs,
        train_ambiguous,
    }
}

/// Names: rescue a row from a dir-level ambiguity where the row itself
/// verified in exactly one pack ("where useful").
fn rescue_names(
    rows: &[Row],
    hits: &[BTreeSet<String>],
    ambiguous: &BTreeMap<String, Vec<String>>,
) -> BTreeMap<String, String> {
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    for (i, r) in rows.iter().enumerate() {
        if ambiguous.contains_key(r.dir.as_str()) && hits[i].len() == 1 {
            names.insert(r.name.clone(), hits[i].iter().next().unwrap().clone());
        }
    }
    names
}

/// Roll the run's counts into a [`Report`].
fn summarize(
    rows: &[Row],
    hits: &[BTreeSet<String>],
    agg: &Aggregated,
    decoded_packs: usize,
    decode_failed: Vec<String>,
) -> Report {
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    for r in rows {
        *kinds.entry(r.kind.to_string()).or_default() += 1;
    }
    let matched = rows.iter().enumerate().filter(|(i, _)| !hits[*i].is_empty()).count();
    Report {
        packs: decoded_packs,
        decode_failed,
        rows: rows.len(),
        matched,
        kinds,
        dirs: agg.dir_count,
        resolved: agg.dirs.len(),
        unverified: agg.unverified.len(),
        ambiguous: agg.ambiguous.clone(),
        train_dirs: agg.train_dirs,
        train_ambiguous: agg.train_ambiguous,
    }
}

pub fn run(data_dir: &Path, exe: Option<&Path>, out: Option<&Path>, check: bool) -> Result<()> {
    let (build, exe_path, index, report) = generate(data_dir, exe)?;
    let id = build.id.clone();

    // ---- emit
    let full = emit_table(&build, &exe_path, &index)?;

    // ---- report (also with --check; the empirical ambiguity answer)
    eprint!("{}", report_text(&report, &id, data_dir));

    write_or_check(out, check, &full, data_dir, &report)?;
    Ok(())
}

/// The full `dirindex.toml` text: the provenance header + the serialized index.
fn emit_table(
    build: &a9_mod::buildid::BuildId,
    exe_path: &Path,
    index: &a9_mod::dirindex::DirIndex,
) -> Result<String> {
    let id = build.id.clone();
    let mut t = String::new();
    t.push_str("# a9loader per-build dir index — the `host = \"auto\"` resolution table.\n");
    t.push_str("#\n");
    t.push_str("# Generated by `a9ml gen-dirindex`; regenerate on a game update. A-Train 9's\n");
    t.push_str("# i.dat row carries a bare name, a dir, and an (off, len) that selects bytes in\n");
    t.push_str("# whichever pack is resident when the name is queried — there is no per-row pack\n");
    t.push_str("# tag. This table recovers that host pack empirically: every shipped row was\n");
    t.push_str("# tested against every decoded pack buffer (ADX0 chunk headers for .x, image\n");
    t.push_str("# magics for textures, shape heuristics for text-like kinds) and aggregated per\n");
    t.push_str("# dir. `dirs` = the pack a dir's rows verify in; `ambiguous` = a dir whose rows\n");
    t.push_str("# span packs (its consumers span boot windows → `host = \"auto\"` refuses);\n");
    t.push_str("# `names` = exact-name rescues for rows inside an ambiguous dir. Absent dirs are\n");
    t.push_str("# unknown. See crates/a9-mod/src/dirindex.rs for the resolution rules.\n");
    t.push_str(&format!(
        "# Provenance: exe {}, FILEVERSION {} (build id {id}), generated {} UTC.\n#\n",
        exe_path.display(),
        build.tuple(),
        civil_stamp()
    ));
    let body = toml::to_string(index).context("serializing the dir index")?;
    Ok(t + &body)
}

/// The diagnostic report text (goes to stderr, so stdout stays clean TOML when
/// no `--out` is given).
fn report_text(report: &Report, id: &str, data_dir: &Path) -> String {
    let mut buf = String::new();
    buf.push_str(&format!("gen-dirindex: build {id} from {}\n", data_dir.display()));
    buf.push_str(&format!("   packs decoded: {}\n", report.packs));
    if !report.decode_failed.is_empty() {
        buf.push_str(&format!("   packs NOT decoded: {}\n", report.decode_failed.join(", ")));
    }
    buf.push_str(&format!(
        "   rows: {} ({}), matched {}, unmatched {}\n",
        report.rows,
        report
            .kinds
            .iter()
            .map(|(k, n)| format!("{k} {n}"))
            .collect::<Vec<_>>()
            .join(", "),
        report.matched,
        report.rows - report.matched
    ));
    buf.push_str(&format!(
        "   dirs: {} — {} resolved, {} ambiguous, {} unverified (left out)\n",
        report.dirs,
        report.resolved,
        report.ambiguous.len(),
        report.unverified
    ));
    if report.ambiguous.is_empty() {
        buf.push_str("   ambiguous dirs: NONE — every shipped dir verifies in exactly one pack\n");
    } else {
        buf.push_str(&format!("   ambiguous dirs ({}):\n", report.ambiguous.len()));
        for (d, ps) in &report.ambiguous {
            buf.push_str(&format!("     {d:?} -> [{}]\n", ps.join(", ")));
        }
    }
    buf.push_str(&format!(
        "   train family: {} dir(s), ambiguous {} (a new `train\\<name>` dir resolves via the `train` prefix)\n",
        report.train_dirs, report.train_ambiguous
    ));
    buf
}

/// Write `--out`, or verify it under `--check`, or print to stdout.
fn write_or_check(
    out: Option<&Path>,
    check: bool,
    full: &str,
    data_dir: &Path,
    report: &Report,
) -> Result<()> {
    if let Some(path) = out {
        if check {
            let cur = std::fs::read_to_string(path).unwrap_or_default();
            let norm = |s: &str| -> String {
                s.lines()
                    .filter(|l| {
                        let l = l.trim_start();
                        !l.starts_with('#') && !l.is_empty()
                    })
                    .map(|l| l.trim().to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            if norm(&cur) != norm(full) {
                bail!(
                    "{} differs from the generated table — regenerate it \
                     (a9ml gen-dirindex --data-dir {} --out {})",
                    path.display(),
                    data_dir.display(),
                    path.display()
                );
            }
            println!("check ok: {} matches {}", path.display(), data_dir.display());
        } else {
            std::fs::write(path, full)
                .with_context(|| format!("writing {}", path.display()))?;
            eprintln!("   wrote {} ({} dirs)", path.display(), report.resolved);
        }
    } else if !check {
        print!("{full}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_classification() {
        assert_eq!(Kind::of("_t_1_a.x"), Kind::X);
        assert_eq!(Kind::of("A.X"), Kind::X);
        assert_eq!(Kind::of("light.dir"), Kind::Dir);
        assert_eq!(Kind::of("train_form.csv"), Kind::Csv);
        assert_eq!(Kind::of("_d_x.dds"), Kind::Tex);
        assert_eq!(Kind::of("previewtexture.bmp"), Kind::Tex);
        assert_eq!(Kind::of("config.att"), Kind::Att);
        assert_eq!(Kind::of("mystery"), Kind::Other);
    }

    #[test]
    fn x_row_needs_adx0_and_matching_length() {
        let mut buf = vec![0u8; 40];
        buf[0..4].copy_from_slice(b"ADX0");
        // payload length at +8 (24-bit) == len - 12
        buf[8..12].copy_from_slice(&(28u32).to_le_bytes());
        assert!(row_matches(Kind::X, 0, 40, &buf));
        // a length mismatch fails
        assert!(!row_matches(Kind::X, 0, 41, &buf));
        // a different offset without the magic fails
        assert!(!row_matches(Kind::X, 1, 39, &buf));
        // off+len past the buffer fails
        assert!(!row_matches(Kind::X, 0, 41, &buf[..40]));
        // the low 24 bits only: the top byte is ignored
        buf[11] = 0xff;
        assert!(row_matches(Kind::X, 0, 40, &buf));
    }

    #[test]
    fn tex_row_needs_an_image_magic() {
        assert!(row_matches(Kind::Tex, 0, 8, b"DDS \x00\x00\x00\x00"));
        assert!(row_matches(Kind::Tex, 0, 4, b"BM  "));
        assert!(row_matches(Kind::Tex, 0, 4, b"\x89PNG"));
        assert!(!row_matches(Kind::Tex, 0, 4, b"nope"));
    }

    #[test]
    fn text_kinds_and_dir_run() {
        assert!(row_matches(Kind::Att, 0, 11, b"hello world"));
        assert!(!row_matches(Kind::Att, 0, 4, &[0, 1, 2, 3]));
        // a UTF-16 ASCII run reads as a .dir name list
        let mut b = Vec::new();
        for u in "abc".encode_utf16() {
            b.extend_from_slice(&u.to_le_bytes());
        }
        b.extend_from_slice(&[0, 0]);
        assert!(row_matches(Kind::Dir, 0, 8, &b));
        assert!(!row_matches(Kind::Dir, 0, 8, &[1, 2, 3, 4, 5, 6, 7, 8]));
    }

    #[test]
    fn aggregation_picks_the_full_coverage_owner() {
        let h = |v: &[&str]| -> BTreeSet<String> { v.iter().map(|s| s.to_string()).collect() };
        // strong rows: vp carries all three, cp one coincidental
        let hits = vec![h(&["vp", "cp"]), h(&["vp"]), h(&["vp"])];
        let kinds = vec![Kind::X, Kind::X, Kind::Tex];
        match resolve_dir(&hits, &kinds) {
            DirResolution::Host(p) => assert_eq!(p, "vp"),
            _ => panic!("expected vp"),
        }
        // two packs each fully cover part of the rows -> ambiguous
        let hits = vec![h(&["vp"]), h(&["cp"])];
        let kinds = vec![Kind::X, Kind::X];
        match resolve_dir(&hits, &kinds) {
            DirResolution::Ambiguous(ps) => assert_eq!(ps.into_iter().collect::<Vec<_>>(), vec!["cp", "vp"]),
            _ => panic!("expected ambiguous"),
        }
        // no pack at all -> unverified
        let hits = vec![h(&[]), h(&[])];
        let kinds = vec![Kind::X, Kind::X];
        assert!(matches!(resolve_dir(&hits, &kinds), DirResolution::Unverified));
        // a single pack is the owner even with no full coverage
        let hits = vec![h(&["vp"]), h(&["vp", "cp"])];
        let kinds = vec![Kind::X, Kind::X];
        match resolve_dir(&hits, &kinds) {
            DirResolution::Host(p) => assert_eq!(p, "vp"),
            _ => panic!("expected vp"),
        }
        // with no strong rows, weak rows decide (all match mcp)
        let hits = vec![h(&["mcp"]), h(&["mcp"])];
        let kinds = vec![Kind::Att, Kind::Dir];
        match resolve_dir(&hits, &kinds) {
            DirResolution::Host(p) => assert_eq!(p, "mcp"),
            _ => panic!("expected mcp"),
        }
    }

    /// Real-data end-to-end, gated on the game data dir like the other
    /// real-data tests (`A9_CSV_TABLE`, `A9_STEAM_MTENTR0`).
    #[test]
    fn real_data_when_available() {
        let Ok(dir) = std::env::var("A9_DATA_DIR") else {
            return;
        };
        let dir = PathBuf::from(dir);
        if !dir.join("i.dat").is_file() {
            return;
        }
        // the generator must produce a table whose train family is clean and
        // whose primary case (a brand-new train dir) resolves to vp
        let (_, bytes) = find_exe(&dir, None).expect("exe next to the data dir");
        let build = a9_mod::buildid::from_exe_bytes(&bytes).unwrap();
        let idat = fmt::idat::parse(
            &fmt::pack::decode_pack(&std::fs::read(dir.join("i.dat")).unwrap(), fmt::pack::Family::A)
                .unwrap(),
        )
        .unwrap();
        assert!(!idat.entries.is_empty());
        let _ = build;
    }
}
