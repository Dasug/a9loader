//! `a9ml verify` — prove the format library against real packs.
//!
//! Runs the synthetic self-tests ([`super::selftests`]), then sweeps every
//! recognized pack in the given data dirs: decrypt + decode + container
//! invariants + our-encoder round-trip + rebuild parity, optionally
//! byte-differentially against the C oracle (`a9enc` / `a9dec`). A pack that
//! is not a recognized pack basename is reported as skipped.
//!
//! [`run`] is the orchestrator: self-tests → oracle resolution → one
//! [`sweep_dir`] per data dir → the `--expect-rebuild` completeness check →
//! the verdict. The per-pack checks live in [`check_pack`].

use a9_formats as fmt;
use anyhow::{bail, Context, Result};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::selftests::self_tests;

/// One named pass/fail line in the verify report. Constructed by the
/// self-tests ([`super::selftests`]) as well as the sweep.
pub(super) struct Check {
    name: &'static str,
    ok: bool,
    note: String,
}

impl Check {
    pub(super) fn ok(name: &'static str, note: impl Into<String>) -> Self {
        Self { name, ok: true, note: note.into() }
    }
    pub(super) fn fail(name: &'static str, note: impl Into<String>) -> Self {
        Self { name, ok: false, note: note.into() }
    }
}

/// A `--expect-rebuild` entry: `KEY` (every swept dir) or `KEY@DIRSUBSTR`
/// (only data dirs whose path contains DIRSUBSTR) — e.g. `csvp@frybarec`
/// scopes the expectation to the Steam install so the *original* JP csvp
/// (a different, ARTDINK-encoded file) cannot false-fail it.
struct Expect {
    key: String,
    dir: Option<String>,
    seen: bool,
}

pub fn run(
    data_dirs: &[PathBuf],
    oracle: Option<&Path>,
    expect_rebuild: &[String],
    only: &[String],
) -> Result<()> {
    let mut failed = run_selftests();

    let mut expects = parse_expectations(expect_rebuild);
    let oracle = resolve_oracle(oracle)?;

    for dir in data_dirs {
        failed += sweep_dir(dir, only, &mut expects, oracle)?;
    }

    // --expect-rebuild completeness: every expectation must have matched a
    // swept pack somewhere (typo protection)
    failed += count_unmatched_expectations(&expects);
    print_expect_keys(&expects);

    let verdict = if failed == 0 { "ALL GREEN".to_string() } else { format!("{failed} FAILURES") };
    println!("== {verdict}");
    if failed > 0 {
        bail!("{failed} check(s) failed");
    }
    Ok(())
}

/// Run the synthetic self-tests and print their lines; returns the failures.
fn run_selftests() -> usize {
    let mut failed = 0usize;
    println!("== a9ml verify — self-tests (synthetic)");
    for c in self_tests() {
        report(c.name, c.ok, &c.note);
        failed += !c.ok as usize;
    }
    failed
}

/// Parse `--expect-rebuild` values into the completeness-checked entries.
fn parse_expectations(expect_rebuild: &[String]) -> Vec<Expect> {
    expect_rebuild
        .iter()
        .map(|s| match s.split_once('@') {
            Some((k, d)) => Expect {
                key: k.trim().to_ascii_lowercase(),
                dir: Some(d.trim().to_string()),
                seen: false,
            },
            None => Expect {
                key: s.trim().to_ascii_lowercase(),
                dir: None,
                seen: false,
            },
        })
        .collect()
}

/// Validate `--oracle` (both `a9enc` and `a9dec` must be there) and announce
/// it; `None` means no differential checks.
fn resolve_oracle(oracle: Option<&Path>) -> Result<Option<&Path>> {
    match oracle {
        Some(dir) => {
            let enc = dir.join("a9enc");
            let dec = dir.join("a9dec");
            if !enc.is_file() || !dec.is_file() {
                bail!("--oracle {}: a9enc/a9dec not found there", dir.display());
            }
            println!("== oracle: {} (a9enc + a9dec)", dir.display());
            Ok(Some(dir))
        }
        None => {
            println!("== oracle: none (no differential byte-parity checks)");
            Ok(None)
        }
    }
}

/// Sweep one data dir; returns the number of failed checks. Marks every
/// matched `--expect-rebuild` entry as seen.
fn sweep_dir(
    dir: &Path,
    only: &[String],
    expects: &mut [Expect],
    oracle: Option<&Path>,
) -> Result<usize> {
    println!("== sweeping {}", dir.display());
    let dir_str = dir.to_string_lossy().to_string();
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    entries.sort();
    let mut failed = 0usize;
    let mut skipped: Vec<String> = Vec::new();
    for path in &entries {
        let key = fmt::pack::classify_key(&path.file_name().unwrap().to_string_lossy());
        let Some((family, container)) = fmt::pack::classify(
            &path.file_name().unwrap().to_string_lossy(),
        ) else {
            skipped.push(key);
            continue;
        };
        if !only.is_empty() && !only.iter().any(|o| o.eq_ignore_ascii_case(&key)) {
            continue;
        }
        let file = fs::read(path).context("reading pack")?;
        let (checks, expect_idx) = check_pack(&key, family, container, &file, oracle, &dir_str, expects)?;
        if let Some(i) = expect_idx {
            expects[i].seen = true;
        }
        failed += checks.iter().any(|c| !c.ok) as usize;
        print_row(&key, &checks);
    }
    if !skipped.is_empty() {
        println!("   skipped (not classified as packs): {}", skipped.join(", "));
    }
    Ok(failed)
}

/// Every check for one pack: decode, container invariants, our-encoder
/// round-trip, rebuild parity and (when given) the oracle differentials.
/// Returns the checks and the `--expect-rebuild` entry index this pack
/// satisfies (if any).
fn check_pack(
    key: &str,
    family: fmt::pack::Family,
    container: fmt::pack::Container,
    file: &[u8],
    oracle: Option<&Path>,
    dir_str: &str,
    expects: &[Expect],
) -> Result<(Vec<Check>, Option<usize>)> {
    let mut checks: Vec<Check> = Vec::new();
    if file.len() < 24 {
        checks.push(Check::ok("stub", format!("{} bytes — skipped (0-byte stub)", file.len())));
        return Ok((checks, None));
    }
    // decode + integrity
    let decoded = match fmt::pack::decode_pack(file, family) {
        Ok(d) => d,
        Err(e) => {
            checks.push(Check::fail("decode", e.to_string()));
            return Ok((checks, None));
        }
    };
    let plain = fmt::crypt::decrypt_body(file).unwrap();
    let declared = if family == fmt::pack::Family::A {
        Some(u32::from_le_bytes(plain[..4].try_into().unwrap()) as usize)
    } else {
        None
    };
    checks.push(Check::ok("decode", format_size_note(decoded.len(), declared)));
    checks.extend(check_container(container, &decoded));
    // our-encoder round-trip
    let lz = fmt::lzss::encode(&decoded);
    if fmt::lzss::decode(&lz, 0) == decoded {
        checks.push(Check::ok("roundtrip", format!("re-encoded {} -> {}", decoded.len(), lz.len())));
    } else {
        checks.push(Check::fail("roundtrip", "decode(encode(x)) != x"));
    }
    // rebuild parity (informational unless --expect-rebuild)
    let rebuilt = fmt::pack::encode_pack(&decoded, family);
    let rebuild_match = rebuilt == file;
    let expect_idx = expects
        .iter()
        .position(|ex| ex.key == key && ex.dir.as_deref().map_or(true, |d| dir_str.contains(d)));
    if rebuild_match {
        checks.push(Check::ok("rebuild", "byte-identical re-materialization"));
    } else {
        let note = format!(
            "differs (original {} B, ours {} B)",
            file.len(),
            rebuilt.len()
        );
        if expect_idx.is_some() {
            checks.push(Check::fail("rebuild", note));
        } else {
            checks.push(Check::ok("rebuild*", note)); // '*' = info only
        }
    }
    // oracle differentials
    if let Some(oracle) = oracle {
        let skip = if family == fmt::pack::Family::A { 4 } else { 0 };
        let od = oracle_decode(oracle, file, skip, key)
            .with_context(|| format!("oracle a9dec failed for {key}"))?;
        if od == decoded {
            checks.push(Check::ok("oracle-dec", "a9dec byte-identical"));
        } else {
            checks.push(Check::fail("oracle-dec", "a9dec output differs"));
        }
        let oe = oracle_encode(oracle, &decoded, key)
            .with_context(|| format!("oracle a9enc failed for {key}"))?;
        if oe == lz {
            checks.push(Check::ok("oracle-enc", "a9enc byte-identical"));
        } else {
            checks.push(Check::fail("oracle-enc", "a9enc output differs"));
        }
    }
    Ok((checks, expect_idx))
}

/// Container invariants + serialize parity for one decoded pack.
fn check_container(container: fmt::pack::Container, decoded: &[u8]) -> Vec<Check> {
    match container {
        fmt::pack::Container::Images => match fmt::container::parse_images(decoded) {
            Ok(entries) => {
                let n = entries.len();
                if fmt::container::serialize_images(&entries) == decoded {
                    vec![Check::ok("container", format!("{n} image entries"))]
                } else {
                    vec![Check::fail("container", "serialize != decoded")]
                }
            }
            Err(e) => vec![Check::fail("container", e.to_string())],
        },
        fmt::pack::Container::Csvs => match fmt::container::parse_csvs(decoded) {
            Ok(entries) => {
                let n = entries.len();
                if fmt::container::serialize_csvs(&entries) == decoded {
                    vec![Check::ok("container", format!("{n} csv entries"))]
                } else {
                    vec![Check::fail("container", "serialize != decoded")]
                }
            }
            Err(e) => vec![Check::fail("container", e.to_string())],
        },
        fmt::pack::Container::Idat => match fmt::idat::parse(decoded) {
            Ok(idat) => {
                let n = idat.entries.len();
                if fmt::idat::serialize(&idat) == decoded {
                    vec![Check::ok("container", format!("{n} i.dat rows"))]
                } else {
                    vec![Check::fail("container", "serialize != decoded")]
                }
            }
            Err(e) => vec![Check::fail("container", e.to_string())],
        },
        fmt::pack::Container::Opaque => {
            vec![Check::ok("container", "opaque (family-A stream)")]
        }
    }
}

/// Print + count the `--expect-rebuild` entries that never matched.
fn count_unmatched_expectations(expects: &[Expect]) -> usize {
    let mut failed = 0usize;
    for ex in expects {
        if !ex.seen {
            println!(
                "   expectation '{}'{} never matched a swept pack",
                ex.key,
                ex.dir.as_deref().map(|d| format!("@{d}")).unwrap_or_default()
            );
            failed += 1;
        }
    }
    failed
}

/// Print the `--expect-rebuild` key list (when any were given).
fn print_expect_keys(expects: &[Expect]) {
    if expects.is_empty() {
        return;
    }
    let list = expects
        .iter()
        .map(|ex| match &ex.dir {
            Some(d) => format!("{}@{}", ex.key, d),
            None => ex.key.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    println!("== expect-rebuild keys: {list}");
}

/// The `decode` note: decoded size, and (family A) the declared output size.
fn format_size_note(decoded: usize, declared: Option<usize>) -> String {
    match declared {
        Some(d) if d != decoded => format!("{decoded} B decoded (declared {d})"),
        Some(_) => format!("{decoded} B decoded (declared matches)"),
        None => format!("{decoded} B decoded"),
    }
}

/// One `[ ok ]/[FAIL] name: note` line (used by the self-tests).
fn report(name: &str, ok: bool, note: &str) {
    println!("   [{}] {name}: {note}", if ok { " ok " } else { "FAIL" });
}

/// One pack's row: `key  check=ok ...` plus the `! name: note` lines for the
/// failed checks. Flushes so a long sweep streams.
fn print_row(key: &str, checks: &[Check]) {
    let joined = checks
        .iter()
        .map(|c| format!("{}={}", c.name, if c.ok { "ok" } else { "FAIL" }))
        .collect::<Vec<_>>()
        .join(" ");
    let marks = if checks.iter().all(|c| c.ok) { "" } else { "  <-- FAIL" };
    println!("   {key:<12} {joined}{marks}");
    for c in checks.iter().filter(|c| !c.ok) {
        println!("      ! {}: {}", c.name, c.note);
    }
    let _ = std::io::stdout().flush();
}

// ---------------------------------------------------------------- oracle

fn tmp_for(tag: &str, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!("a9ml-verify-{}.{ext}", tag.replace('/', "_")))
}

fn oracle_encode(oracle: &Path, data: &[u8], tag: &str) -> Result<Vec<u8>> {
    let inp = tmp_for(&format!("{tag}-in"), "bin");
    let outp = tmp_for(&format!("{tag}-out"), "lz");
    fs::write(&inp, data)?;
    let st = Command::new(oracle.join("a9enc"))
        .arg(&inp)
        .arg(&outp)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    let _ = fs::remove_file(&inp);
    if !st.success() {
        let _ = fs::remove_file(&outp);
        bail!("a9enc exited with {st}");
    }
    let out = fs::read(&outp)?;
    let _ = fs::remove_file(&outp);
    Ok(out)
}

fn oracle_decode(oracle: &Path, file: &[u8], skip: usize, tag: &str) -> Result<Vec<u8>> {
    let inp = tmp_for(&format!("{tag}-in"), "bin");
    let outp = tmp_for(&format!("{tag}-out"), "dec");
    fs::write(&inp, file)?;
    let st = Command::new(oracle.join("a9dec"))
        .arg(&inp)
        .arg(&outp)
        .arg(skip.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    let _ = fs::remove_file(&inp);
    if !st.success() {
        let _ = fs::remove_file(&outp);
        bail!("a9dec exited with {st}");
    }
    let out = fs::read(&outp)?;
    let _ = fs::remove_file(&outp);
    Ok(out)
}
