//! `a9ml extract-asset` / `a9ml merge-assets` — the shipped-asset authoring
//! pair.
//!
//! `extract-asset` dumps one shipped `i.dat` row's raw payload. A-Train 9's
//! master index (`i.dat`) is the only place a shipped asset's bytes are named:
//! each row carries a bare `name`, a consumer `dir`, and an `(off, len)` that
//! selects bytes in **whichever pack is resident when the name is queried** —
//! there is no per-row pack tag (`notes/03`, `notes/06`). The command recovers
//! the owning pack and writes the payload verbatim, so an author can take a
//! shipped asset apart (a `previewtexture.dir`, a preview DDS, a mesh) and
//! build a mod payload from it.
//!
//! Host resolution, most authoritative first:
//! 1. `--pack` (explicit; the caller knows);
//! 2. the embedded per-build `dir -> pack` index — the same
//!    [`a9_mod::dirindex::resolve_auto`] the loader uses for `host = "auto"`;
//! 3. a **content scan** over every family-A pack: the payload at `(off, len)`
//!    must pass a strong per-kind check (an image magic, the `ADX0` chunk
//!    length, a whole-blob UTF-16 name list for `.dir`, …). This is what
//!    rescues a row whose `dir` is ambiguous in the index (e.g.
//!    `previewtexture.dir`, whose own `dir` field is `previewtexture.dir` and
//!    which the index lists as spanning `me2` + `tp`).
//!
//! Only family-A packs (`vp`/`tp`/`mcp`/`ep`/`cp`) carry rows addressed by
//! `(off, len)`; the index resolves no `dir` to a family-B pack, so a scan is
//! the right fallback. Family-B packs (`me*`/`mt*`/`csvp`/…) are addressed by
//! an in-pack entry table, not by `i.dat`, and are out of scope here.
//!
//! `merge-assets` is the write-side dry-run: it builds the exact merged
//! `<host>.bin` pack(s) and merged `i.dat` a single mod's `[[asset]]` sections
//! produce, using the same `a9_formats` primitives the loader runs — nothing is
//! installed. (Multi-mod overlap resolution stays `a9ml list`'s job.)

use a9_formats as fmt;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// A row's payload plus how it was found (for the report).
#[derive(Debug)]
pub struct Extracted {
    pub name: String,
    pub dir: String,
    pub host: String,
    pub off: u32,
    pub len: u32,
    pub bytes: Vec<u8>,
    /// "explicit" | "dir index" | "scan"
    pub how: &'static str,
}

/// The `data` dir for `game`: `game/data` when it holds `i.dat`, else `game`
/// itself (so both the game dir and the data dir are accepted).
fn data_dir(game: &Path) -> Result<PathBuf> {
    let nested = game.join("data");
    if nested.join("i.dat").is_file() {
        return Ok(nested);
    }
    if game.join("i.dat").is_file() {
        return Ok(game.to_path_buf());
    }
    bail!(
        "no i.dat under {} (looked for {}/data/i.dat and {}/i.dat)",
        game.display(),
        game.display(),
        game.display()
    )
}

/// Parse the shipped `i.dat` of a data dir.
fn shipped_idat(data: &Path) -> Result<fmt::idat::Idat> {
    let path = data.join("i.dat");
    let file = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let decoded = fmt::pack::decode_pack(&file, fmt::pack::Family::A)
        .with_context(|| format!("decoding {}", path.display()))?;
    fmt::idat::parse(&decoded).with_context(|| format!("parsing {}", path.display()))
}

/// Find an `ATrain9*.exe` in `dir` (the build id keys the dir index).
fn find_exe(dir: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
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
    candidates.sort_by_key(|p| {
        let n = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        (if n == "ATrain9v5.exe" { 0 } else { 1 }, n)
    });
    candidates.into_iter().next()
}

/// Does `bytes` plausibly BE a payload of the kind `name` implies? Stricter
/// than the dir-index heuristic (which only samples the head): a scan
/// candidate must be self-consistent over the whole blob, so a false positive
/// in another pack's buffer is unlikely.
pub fn payload_kind_ok(name: &str, bytes: &[u8]) -> bool {
    let n = name.to_ascii_lowercase();
    if n.ends_with(".dds") {
        return bytes.len() >= 128
            && bytes.starts_with(b"DDS ")
            && u32::from_le_bytes(bytes[4..8].try_into().unwrap()) == 124;
    }
    if n.ends_with(".bmp") {
        return bytes.len() >= 2 && bytes.starts_with(b"BM");
    }
    if n.ends_with(".png") {
        return bytes.starts_with(b"\x89PNG");
    }
    if n.ends_with(".tga") {
        // uncompressed/RLE truecolor (the game's sky/terrain TGAs): cmap 0/1,
        // image type in the common set.
        return bytes.len() >= 18
            && matches!(bytes[2], 0 | 1)
            && matches!(bytes[1], 1 | 2 | 3 | 9 | 10 | 11);
    }
    if n.ends_with(".x") {
        // `ADX0` chunk header + the 24-bit payload length at +8
        return bytes.len() >= 12
            && &bytes[..4] == b"ADX0"
            && (u32::from_le_bytes(bytes[8..12].try_into().unwrap()) & 0x00ff_ffff)
                == (bytes.len() as u32).wrapping_sub(12);
    }
    if n.ends_with(".dir") {
        return utf16_name_list_ok(bytes);
    }
    // text-like (`.att`/`.csv`/`.dat`/other): the whole blob must be mostly
    // printable (the `.dat` blobs are binary — fall back to a looser bound).
    if n.ends_with(".dat") {
        return bytes.len() >= 4;
    }
    ascii_like(bytes)
}

/// A `.dir` blob: a run of NUL-terminated UTF-16 names. Every unit is NUL or a
/// printable/whitespace character, and the run is NUL-terminated.
pub fn utf16_name_list_ok(b: &[u8]) -> bool {
    if b.len() < 4 || b.len() % 2 != 0 {
        return false;
    }
    let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    if units.iter().all(|&u| u != 0) {
        return false; // a name list always has terminators
    }
    units
        .iter()
        .all(|&u| u == 0 || u == 9 || u == 10 || u == 13 || (0x20..=0x7e).contains(&u))
}

fn ascii_like(b: &[u8]) -> bool {
    let good = b
        .iter()
        .filter(|&&c| (9..=13).contains(&c) || (32..=126).contains(&c))
        .count();
    good * 10 >= b.len().max(1) * 9
}

/// Decode every family-A pack in `data` and return those whose bytes at
/// `(off, len)` pass the kind check — the `host = "auto"` fallback.
fn scan_host(data: &Path, name: &str, off: u32, len: u32) -> Result<Vec<String>> {
    let (off, len) = (off as usize, len as usize);
    let rd = std::fs::read_dir(data).map_err(|e| anyhow::anyhow!("reading {}: {e}", data.display()))?;
    let mut files: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_file()).collect();
    files.sort();
    let mut hits = Vec::new();
    for p in files {
        let fname = p.file_name().unwrap().to_string_lossy().to_string();
        let Some((family, container)) = fmt::pack::classify(&fname) else {
            continue;
        };
        // only the family-A opaque streams carry `(off, len)` rows
        if family != fmt::pack::Family::A || container != fmt::pack::Container::Opaque {
            continue;
        }
        let Ok(file) = std::fs::read(&p) else { continue };
        let Ok(buf) = fmt::pack::decode_pack(&file, family) else { continue };
        let Some(end) = off.checked_add(len) else { continue };
        let Some(blob) = buf.get(off..end) else { continue };
        if payload_kind_ok(name, blob) {
            hits.push(fmt::pack::classify_key(&fname));
        }
    }
    Ok(hits)
}

/// Resolve the host pack of a row (`name`, `dir`).
fn resolve_host(
    game: &Path,
    data: &Path,
    name: &str,
    dir: &str,
    off: u32,
    len: u32,
    explicit: Option<&str>,
) -> Result<(String, &'static str)> {
    if let Some(p) = explicit {
        return Ok((p.to_ascii_lowercase(), "explicit"));
    }
    // the embedded dir index (the loader's own `host = "auto"` resolver)
    if let Some(exe) = find_exe(game) {
        if let Ok(bytes) = std::fs::read(&exe) {
            if let Some(build) = a9_mod::buildid::from_exe_bytes(&bytes) {
                if let Ok(idx) = a9_mod::dirindex::embedded_index() {
                    if let Ok(h) = a9_mod::dirindex::resolve_auto(&idx, &build.id, name, dir) {
                        return Ok((h, "dir index"));
                    }
                }
            }
        }
    }
    // content scan fallback
    let hits = scan_host(data, name, off, len)?;
    match hits.as_slice() {
        [one] => Ok((one.clone(), "scan")),
        [] => bail!(
            "cannot locate a host pack for {name:?} (off={off} len={len}): no dir-index \
             answer and no family-A pack's bytes at that offset look like a {name} payload — \
             pass --pack to name the host explicitly"
        ),
        many => bail!(
            "host for {name:?} is ambiguous: {} packs hold a plausible payload at \
             (off={off}, len={len}) — pass --pack to pick one",
            many.join(", ")
        ),
    }
}

/// Extract one `i.dat` row's payload from its host pack.
pub fn extract(game: &Path, name: &str, explicit: Option<&str>) -> Result<Extracted> {
    let data = data_dir(game)?;
    let idat = shipped_idat(&data)?;
    let row = idat.row(name).with_context(|| {
        let sample: Vec<&str> = idat.entries.iter().take(5).map(|e| e.name.as_str()).collect();
        format!(
            "{name:?} is not a shipped i.dat row ({} rows; e.g. {})",
            idat.entries.len(),
            sample.join(", ")
        )
    })?;
    let (off, len) = (row.v3(), row.v1());
    let dir = row.dir.clone();
    let (host, how) = resolve_host(game, &data, name, &dir, off, len, explicit)?;

    let (family, container) = fmt::pack::classify(&host)
        .with_context(|| format!("host {host:?} is not a recognized pack"))?;
    if family != fmt::pack::Family::A || container != fmt::pack::Container::Opaque {
        bail!(
            "host {host:?} for {name:?} is not a family-A stream — extract-asset reads \
             `(off, len)` rows, which only vp/tp/mcp/ep/cp carry"
        );
    }
    let path = data.join(format!("{host}.bin"));
    let file = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let buf = fmt::pack::decode_pack(&file, family)
        .with_context(|| format!("decoding {}", path.display()))?;
    let end = (off as usize)
        .checked_add(len as usize)
        .filter(|&e| e <= buf.len())
        .with_context(|| {
            format!(
                "{name:?}: (off={off}, len={len}) runs past {host}'s decoded {} bytes",
                buf.len()
            )
        })?;
    let bytes = buf[off as usize..end].to_vec();
    if !payload_kind_ok(name, &bytes) {
        eprintln!(
            "a9ml: warning: {name:?} in {host} at (off={off}, len={len}) does not look like \
             a {name} payload — the host may be wrong (use --pack)"
        );
    }
    Ok(Extracted {
        name: name.to_string(),
        dir,
        host,
        off,
        len,
        bytes,
        how,
    })
}

/// CLI entry: extract a row and write its payload to `out` (or stdout).
pub fn run(game: &Path, name: &str, out: Option<&Path>, pack: Option<&str>) -> Result<()> {
    let x = extract(game, name, pack)?;
    let note = format!(
        "{}: host {} ({}), dir {:?}, off {}, len {}",
        x.name, x.host, x.how, x.dir, x.off, x.len
    );
    match out {
        Some(p) => {
            std::fs::write(p, &x.bytes).with_context(|| format!("writing {}", p.display()))?;
            println!("{note} -> {} ({} B)", p.display(), x.bytes.len());
        }
        None => {
            eprintln!("{note} -> stdout ({} B)", x.bytes.len());
            use std::io::Write as _;
            std::io::stdout().write_all(&x.bytes)?;
        }
    }
    Ok(())
}

// ------------------------------------------------------- merge-assets

/// The loader's family-A decode workspace budget (`notes/03`, `notes/24`): the
/// merged decoded image must stay within 200 MB (the loader refuses ~190 MB).
const BUDGET: usize = 190 * 1024 * 1024;

/// One merged `[[asset]]` row, as the loader's i.dat merge will write it.
#[derive(Debug, Clone)]
pub struct MergedRow {
    pub name: String,
    pub dir: String,
    pub host: String,
    /// offset of the payload inside the merged host pack's decoded stream
    pub off: u32,
    pub len: u32,
    pub override_vanilla: bool,
}

/// The offline `[[asset]]` merge: the exact primitive sequence the loader's
/// `merge_family_a_host` + `merge_idat` run (decode → `plan_family_a` →
/// `overlay_assets_decoded`; `idat::add_rows` → serialize → family-A encode).
/// Nothing is installed — `out` receives the merged `<host>.bin` pack(s) and
/// the merged `i.dat`, so an author can inspect exactly what the game would be
/// served. Pure `a9_formats` math, byte-identical to the loader's.
pub fn merge(game: &Path, mod_dir: &Path, out: &Path) -> Result<Vec<MergedRow>> {
    let data = data_dir(game)?;
    // ---- the manifest (with folder-form assets expanded like the loader)
    let manifest_path = mod_dir.join("mod.toml");
    let text = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let mut m: a9_mod::manifest::Manifest =
        toml::from_str(&text).with_context(|| format!("parsing {}", manifest_path.display()))?;
    m.validate().map_err(|e| anyhow::anyhow!("{}: {e}", manifest_path.display()))?;
    a9_mod::scan::expand_asset_folders(&mut m, mod_dir)
        .map_err(|e| anyhow::anyhow!("{}: {e}", manifest_path.display()))?;
    m.validate().map_err(|e| anyhow::anyhow!("{}: {e}", manifest_path.display()))?;
    if m.asset.is_empty() {
        bail!("mod '{}' has no [[asset]] sections — nothing to merge", m.id);
    }

    let idat = shipped_idat(&data)?;
    let build = find_exe(game)
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| a9_mod::buildid::from_exe_bytes(&b));
    let dirindex = a9_mod::dirindex::embedded_index().ok();

    // ---- resolve every op's host (and refuse a vanilla collision early)
    struct Op {
        host: String,
        name: String,
        dir: String,
        file: PathBuf,
        override_vanilla: bool,
    }
    let mut ops: Vec<Op> = Vec::new();
    for a in &m.asset {
        let host = if a.host.eq_ignore_ascii_case(a9_mod::manifest::HOST_AUTO) {
            let build = build.as_ref().with_context(|| {
                "host = \"auto\" needs the game exe's build id (no ATrain9*.exe found)".to_string()
            })?;
            let idx = dirindex.as_ref().with_context(|| "the embedded dir index failed to parse")?;
            a9_mod::dirindex::resolve_auto(idx, &build.id, &a.name, &a.dir)
                .map_err(|e| anyhow::anyhow!("asset {:?}: host auto refused: {e}", a.name))?
        } else {
            a.host.clone()
        };
        if !matches!(
            fmt::pack::classify(&host),
            Some((fmt::pack::Family::A, fmt::pack::Container::Opaque))
        ) {
            bail!("asset {:?}: host {host:?} is not a family-A stream", a.name);
        }
        if idat.row(&a.name).is_some() && !a.override_vanilla {
            bail!(
                "asset {:?} collides with a SHIPPED i.dat row — set \
                 `override_vanilla = true` to replace it",
                a.name
            );
        }
        ops.push(Op {
            host,
            name: a.name.clone(),
            dir: a.dir.clone(),
            file: mod_dir.join(&a.file),
            override_vanilla: a.override_vanilla,
        });
    }

    // ---- per host, in first-appearance order (the loader's order)
    let mut hosts: Vec<String> = Vec::new();
    for o in &ops {
        if !hosts.iter().any(|h| h == &o.host) {
            hosts.push(o.host.clone());
        }
    }
    std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    let mut rows: Vec<MergedRow> = Vec::new();
    for host in &hosts {
        let path = data.join(format!("{host}.bin"));
        let file = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let decoded = fmt::pack::decode_pack(&file, fmt::pack::Family::A)
            .with_context(|| format!("decoding {}", path.display()))?;
        // read the payloads + build the ops for THIS host, in order
        let mut aops: Vec<fmt::overlay::AssetOp> = Vec::new();
        let mut metas: Vec<&Op> = Vec::new();
        for o in ops.iter().filter(|o| &o.host == host) {
            let payload = std::fs::read(&o.file)
                .with_context(|| format!("reading {}", o.file.display()))?;
            aops.push(fmt::overlay::AssetOp {
                name: o.name.clone(),
                dir: o.dir.clone(),
                payload,
            });
            metas.push(o);
        }
        let plan = fmt::overlay::plan_family_a(decoded.len(), &aops);
        let (merged, report) = fmt::overlay::overlay_assets_decoded(&decoded, &aops, BUDGET)
            .map_err(|e| anyhow::anyhow!("host {host}: merge: {e}"))?;
        let out_pack = out.join(format!("{host}.bin"));
        std::fs::write(&out_pack, &merged).with_context(|| format!("writing {}", out_pack.display()))?;
        for (o, (off, len)) in metas.iter().zip(plan) {
            rows.push(MergedRow {
                name: o.name.clone(),
                dir: o.dir.clone(),
                host: host.clone(),
                off: off as u32,
                len: len as u32,
                override_vanilla: o.override_vanilla,
            });
        }
        println!(
            "{host}: {} op(s), vanilla decoded {} B -> merged decoded {} B ({:.1} MB), \
             written {}",
            aops.len(),
            decoded.len(),
            report.decoded,
            report.decoded as f64 / (1024.0 * 1024.0),
            out_pack.display()
        );
    }

    // ---- the merged index (same rows, same order)
    let replaced = rows.iter().filter(|r| idat.row(&r.name).is_some()).count();
    let new_rows: Vec<fmt::idat::NewRow> = rows
        .iter()
        .map(|r| fmt::idat::NewRow {
            name: r.name.clone(),
            dir: r.dir.clone(),
            len: r.len,
            flag: 0,
            off: r.off,
        })
        .collect();
    let mut idat = idat;
    idat.add_rows(&new_rows).map_err(|e| anyhow::anyhow!("i.dat rows: {e}"))?;
    let out_idat = out.join("i.dat");
    std::fs::write(&out_idat, fmt::pack::encode_pack_a(&fmt::idat::serialize(&idat)))
        .with_context(|| format!("writing {}", out_idat.display()))?;
    println!(
        "i.dat: {} row(s) merged ({} new, {} replaced) -> {}",
        rows.len(),
        rows.len() - replaced,
        replaced,
        out_idat.display()
    );
    Ok(rows)
}

/// CLI entry for `merge-assets`.
pub fn run_merge(game: &Path, mod_dir: &Path, out: &Path) -> Result<()> {
    let rows = merge(game, mod_dir, out)?;
    println!("merged [[asset]] rows:");
    for r in &rows {
        println!(
            "   {:<28} host {:<4} dir {:<16} off {:<10} len {:<8}{}",
            r.name,
            r.host,
            format!("{:?}", r.dir),
            r.off,
            r.len,
            if r.override_vanilla { "  (override_vanilla)" } else { "" }
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("a9ml-asset-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// A synthetic game dir: `data/i.dat` + one family-A host pack carrying a
    /// DDS payload, and a second pack that holds a decoy at the same offset.
    fn fixture(tag: &str) -> (PathBuf, Vec<u8>) {
        let game = tmp(tag);
        let data = game.join("data");
        std::fs::create_dir_all(&data).unwrap();
        // a valid-ish 128-byte DDS: magic + dwSize=124
        let mut dds = vec![0u8; 128];
        dds[..4].copy_from_slice(b"DDS ");
        dds[4..8].copy_from_slice(&124u32.to_le_bytes());
        // vp = [16 bytes junk][dds] ; row off=16 len=128
        let mut vp = vec![0xAAu8; 16];
        vp.extend_from_slice(&dds);
        std::fs::write(data.join("vp.bin"), fmt::pack::encode_pack_a(&vp)).unwrap();
        // cp holds a decoy at the same offset that is NOT a DDS
        let cp = vec![0x55u8; 16 + 128];
        std::fs::write(data.join("cp.bin"), fmt::pack::encode_pack_a(&cp)).unwrap();
        // i.dat with one row
        let mut tail = [0u8; 9];
        tail[0..4].copy_from_slice(&128u32.to_le_bytes());
        tail[5..9].copy_from_slice(&16u32.to_le_bytes());
        let idat = fmt::idat::Idat {
            pad: fmt::idat::PAD_STEAM,
            entries: vec![fmt::idat::Entry {
                name: "_d_test.dds".into(),
                dir: "testdir".into(),
                tail,
            }],
        };
        std::fs::write(
            data.join("i.dat"),
            fmt::pack::encode_pack_a(&fmt::idat::serialize(&idat)),
        )
        .unwrap();
        (game, dds)
    }

    #[test]
    fn extracts_by_scan_when_the_index_cannot_resolve() {
        let (game, dds) = fixture("scan");
        // no exe -> no build id -> the dir index cannot answer -> scan; only vp
        // passes the DDS check, so the decoy in cp is not chosen
        let x = extract(&game, "_d_test.dds", None).unwrap();
        assert_eq!(x.host, "vp");
        assert_eq!(x.how, "scan");
        assert_eq!(x.off, 16);
        assert_eq!(x.len, 128);
        assert_eq!(x.bytes, dds);
        let _ = std::fs::remove_dir_all(&game);
    }

    #[test]
    fn explicit_pack_skips_resolution() {
        let (game, _) = fixture("explicit");
        let x = extract(&game, "_d_test.dds", Some("vp")).unwrap();
        assert_eq!(x.host, "vp");
        assert_eq!(x.how, "explicit");
        // a wrong explicit host is still returned (the caller asked) but the
        // bytes are that pack's slice
        let y = extract(&game, "_d_test.dds", Some("cp")).unwrap();
        assert_eq!(y.host, "cp");
        assert_eq!(y.bytes, vec![0x55u8; 128]);
        let _ = std::fs::remove_dir_all(&game);
    }

    #[test]
    fn unknown_row_is_refused() {
        let (game, _) = fixture("unknown");
        let e = extract(&game, "nope.dds", None).unwrap_err().to_string();
        assert!(e.contains("not a shipped i.dat row"), "{e}");
        let _ = std::fs::remove_dir_all(&game);
    }

    #[test]
    fn dir_kind_needs_a_whole_utf16_name_list() {
        // a real run: "a\0b\0\0" (UTF-16) + terminator
        let mut b = Vec::new();
        for u in "a".encode_utf16() {
            b.extend_from_slice(&u.to_le_bytes());
        }
        b.extend_from_slice(&0u16.to_le_bytes());
        for u in "bc".encode_utf16() {
            b.extend_from_slice(&u.to_le_bytes());
        }
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        assert!(utf16_name_list_ok(&b));
        assert!(payload_kind_ok("previewtexture.dir", &b));
        // binary junk fails
        assert!(!utf16_name_list_ok(&[0x55u8; 61648]));
        // a run with no terminator at all fails
        let mut no_nul = Vec::new();
        for u in "abc".encode_utf16() {
            no_nul.extend_from_slice(&u.to_le_bytes());
        }
        assert!(!utf16_name_list_ok(&no_nul));
    }

    #[test]
    fn x_payload_needs_adx0_and_length() {
        let mut x = vec![0u8; 40];
        x[..4].copy_from_slice(b"ADX0");
        x[8..12].copy_from_slice(&28u32.to_le_bytes());
        assert!(payload_kind_ok("_t_a.x", &x));
        assert!(!payload_kind_ok("_t_a.x", &x[..39]));
    }

    /// The offline `[[asset]]` merge: append two payloads to a family-A host and
    /// synthesize the merged index — byte-exact, and the `.dir` payload parses
    /// back to a name list that contains the appended names.
    #[test]
    fn merge_appends_payloads_and_writes_rows() {
        let game = tmp("merge-game");
        let data = game.join("data");
        std::fs::create_dir_all(&data).unwrap();
        // vanilla tp: 16 bytes of junk; the shipped .dir row points at 8..16
        let vanilla = vec![0x11u8; 16];
        std::fs::write(data.join("tp.bin"), fmt::pack::encode_pack_a(&vanilla)).unwrap();
        let idat = fmt::idat::Idat {
            pad: fmt::idat::PAD_STEAM,
            entries: vec![fmt::idat::Entry {
                name: "previewtexture.dir".into(),
                dir: "previewtexture.dir".into(),
                tail: {
                    let mut t = [0u8; 9];
                    t[0..4].copy_from_slice(&8u32.to_le_bytes()); // len
                    t[5..9].copy_from_slice(&8u32.to_le_bytes()); // off
                    t
                },
            }],
        };
        std::fs::write(data.join("i.dat"), fmt::pack::encode_pack_a(&fmt::idat::serialize(&idat)))
            .unwrap();

        // the mod: one new DDS + an override of the shipped .dir
        let modd = game.join("mod");
        std::fs::create_dir_all(modd.join("assets")).unwrap();
        let mut dds = vec![0u8; 128];
        dds[..4].copy_from_slice(b"DDS ");
        dds[4..8].copy_from_slice(&124u32.to_le_bytes());
        std::fs::write(modd.join("assets/new.dds"), &dds).unwrap();
        let mut dir_payload: Vec<u8> = Vec::new();
        for n in ["form_x_f.dds", "form_x_s.dds", "form_x_s_1.dds"] {
            for u in (n.to_string() + "\0").encode_utf16() {
                dir_payload.extend_from_slice(&u.to_le_bytes());
            }
        }
        std::fs::write(modd.join("assets/prev.dir"), &dir_payload).unwrap();
        std::fs::write(
            modd.join("mod.toml"),
            "id = \"t\"\nname = \"T\"\nversion = \"0.1.0\"\nauthor = \"a\"\n\
             [[asset]]\nname = \"_d_new.dds\"\ndir = \"previewtexture\"\nhost = \"tp\"\nfile = \"assets/new.dds\"\n\
             [[asset]]\nname = \"previewtexture.dir\"\ndir = \"previewtexture\"\nhost = \"tp\"\nfile = \"assets/prev.dir\"\noverride_vanilla = true\n",
        )
        .unwrap();

        let out = game.join("out");
        let rows = merge(&game, &modd, &out).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "_d_new.dds");
        assert_eq!((rows[0].off, rows[0].len), (16, 128));
        assert_eq!(rows[1].name, "previewtexture.dir");
        assert!(rows[1].override_vanilla);
        assert_eq!((rows[1].off, rows[1].len), (144, dir_payload.len() as u32));

        // (a) the merged tp == vanilla ++ dds ++ dir_payload, byte-exact
        let merged = fmt::pack::decode_pack(&std::fs::read(out.join("tp.bin")).unwrap(), fmt::pack::Family::A).unwrap();
        let mut expect = vanilla.clone();
        expect.extend_from_slice(&dds);
        expect.extend_from_slice(&dir_payload);
        assert_eq!(merged, expect);

        // (b) the merged .dir row's bytes parse to a list containing our names
        let row = &merged[rows[1].off as usize..(rows[1].off + rows[1].len) as usize];
        let parsed: Vec<String> = row
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect::<Vec<u16>>()
            .split(|&u| u == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf16_lossy(s))
            .collect();
        assert!(parsed.contains(&"form_x_s_1.dds".to_string()));

        // (c) the merged i.dat carries both rows with the plan's offsets
        let mi = fmt::idat::parse(
            &fmt::pack::decode_pack(&std::fs::read(out.join("i.dat")).unwrap(), fmt::pack::Family::A).unwrap(),
        )
        .unwrap();
        assert_eq!(mi.entries.len(), 2, "one override (in place) + one new row");
        assert_eq!(mi.row("_d_new.dds").unwrap().v3(), rows[0].off);
        assert_eq!(mi.row("_d_new.dds").unwrap().v1(), rows[0].len);
        let e = mi.row("previewtexture.dir").unwrap();
        assert_eq!((e.v3(), e.v1()), (rows[1].off, rows[1].len));
        let _ = std::fs::remove_dir_all(&game);
    }

    /// `merge` refuses a vanilla collision without `override_vanilla`.
    #[test]
    fn merge_refuses_vanilla_collision() {
        let game = tmp("merge-refuse");
        let data = game.join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("tp.bin"), fmt::pack::encode_pack_a(&[0u8; 16])).unwrap();
        let idat = fmt::idat::Idat {
            pad: fmt::idat::PAD_STEAM,
            entries: vec![fmt::idat::Entry {
                name: "previewtexture.dir".into(),
                dir: "previewtexture.dir".into(),
                tail: [0u8; 9],
            }],
        };
        std::fs::write(data.join("i.dat"), fmt::pack::encode_pack_a(&fmt::idat::serialize(&idat))).unwrap();
        let modd = game.join("mod");
        std::fs::create_dir_all(modd.join("assets")).unwrap();
        std::fs::write(modd.join("assets/p"), b"x").unwrap();
        std::fs::write(
            modd.join("mod.toml"),
            "id = \"t\"\nname = \"T\"\nversion = \"0.1.0\"\nauthor = \"a\"\n\
             [[asset]]\nname = \"previewtexture.dir\"\ndir = \"previewtexture\"\nhost = \"tp\"\nfile = \"assets/p\"\n",
        )
        .unwrap();
        let e = merge(&game, &modd, &game.join("out")).unwrap_err().to_string();
        assert!(e.contains("collides with a SHIPPED i.dat row"), "{e}");
        let _ = std::fs::remove_dir_all(&game);
    }
}
