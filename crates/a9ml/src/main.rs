//! a9ml — A-Train 9 mod loader authoring & diagnostics CLI.
//!
//! M1 scope: `verify` (format-library proofs against real packs) and
//! `inspect` (decode + describe one pack). Installing a mod stays a folder
//! drop — there is deliberately no install command.

use a9_formats as fmt;
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

mod asset;
mod classify;
mod gen_builds;
mod gen_dirindex;

#[derive(Parser)]
#[command(name = "a9ml", about = "A-Train 9 mod loader — authoring & diagnostics CLI", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Prove the format library: self-tests, then sweep real pack dirs
    /// (decrypt + decode + container invariants + round-trip + rebuild
    /// parity), optionally byte-differentially against the C oracle
    /// (`a9enc` / `a9dec`).
    Verify {
        /// Game data dir(s) holding packs (repeatable)
        #[arg(long = "data-dir", value_name = "DIR")]
        data_dirs: Vec<PathBuf>,
        /// Dir containing the C oracle binaries `a9enc` and `a9dec`
        #[arg(long, value_name = "DIR")]
        oracle: Option<PathBuf>,
        /// Pack keys (basename, `.bin` stripped) that must re-materialize
        /// byte-identically (comma list, repeatable)
        #[arg(long = "expect-rebuild", value_name = "KEYS", value_delimiter = ',')]
        expect_rebuild: Vec<String>,
        /// Restrict the sweep to these pack keys (comma list, repeatable)
        #[arg(long, value_name = "KEYS", value_delimiter = ',')]
        only: Vec<String>,
    },
    /// Decode one pack file and describe it (family, sizes, container)
    Inspect {
        /// Path to the pack file
        path: PathBuf,
        /// List every entry name, not just the first 40
        #[arg(long)]
        all: bool,
    },
    /// Show the mods of a game dir and how the loader would resolve them
    /// (offline — reads manifests + config, applies nothing)
    List {
        /// Game dir (the dir holding ATrain9v5.exe)
        #[arg(long)]
        game: PathBuf,
    },
    /// Diagnose a game dir: build identity, mod resolution, gate verdicts,
    /// and the loader log tail from the last boot
    Doctor {
        /// Game dir (the dir holding ATrain9v5.exe)
        #[arg(long)]
        game: PathBuf,
    },
    /// Classify each data effect by WHERE the game consumes it during boot —
    /// "will my texture replace actually reach the game?" (`early:<window>` =
    /// consumed inside that boot window — serve with pack_serving=inject or
    /// merge; `pre-shader/pre-csv` = consumed in stage 1; `unknown` = be
    /// conservative)
    Classify {
        /// A mod dir (holding mod.toml) or a mods dir (subdirs with mod.toml)
        path: Option<PathBuf>,
        /// Classify one entry name instead of a manifest
        #[arg(long)]
        entry: Option<String>,
        /// Pack context for --entry (e.g. "me3", "mtentr0", "csvp")
        #[arg(long)]
        pack: Option<String>,
    },
    /// Generate a [[pack]] mod from a base pack and a repacked one: every
    /// added or changed entry becomes a mod.toml [[pack.entry]] + a payload
    /// file. Removed entries are unsupported (listed, error).
    DiffPack {
        /// Pack name without path/extension (e.g. "mtentr0")
        #[arg(long)]
        pack: String,
        /// The original (vanilla) pack file
        #[arg(long)]
        base: PathBuf,
        /// The repacked pack file (your result)
        #[arg(long)]
        modded: PathBuf,
        /// Output mod directory (created; mod.toml + entries/)
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        id: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        author: String,
        /// Optional: game build ids this mod targets
        #[arg(long = "game-build", value_name = "ID")]
        game_builds: Vec<String>,
    },
    /// Generate a [[csv]] mod from a base csvp and a repacked one: every
    /// added or changed table becomes a mod.toml [[csv]] + a CSV payload.
    /// With `--rows`, changed tables ship only their changed/added rows as a
    /// `mode = "rows"` patch instead of the whole table.
    DiffCsv {
        /// The original (vanilla) csvp.bin
        #[arg(long)]
        base: PathBuf,
        /// The repacked csvp.bin (your result)
        #[arg(long)]
        modded: PathBuf,
        /// Output mod directory (created; mod.toml + csv/)
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        id: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        author: String,
        #[arg(long = "game-build", value_name = "ID")]
        game_builds: Vec<String>,
        /// Emit row-level row patches (`mode = "rows"`) for changed tables
        /// instead of whole-table replaces
        #[arg(long)]
        rows: bool,
    },
    /// Regenerate the build address book's core anchors from a live exe
    /// (provenance-stamped; hand-appended RE anchors stay in the file).
    GenBuilds {
        /// The game executable
        #[arg(long)]
        exe: PathBuf,
        /// Output path (prints to stdout when omitted)
        #[arg(long)]
        out: Option<PathBuf>,
        /// Verify that out/... (requires --out) matches this exe exactly
        #[arg(long)]
        check: bool,
    },
    /// Regenerate the per-build `dir -> pack` index (the `host = "auto"`
    /// resolution table) from a live install's i.dat + packs: every shipped
    /// row is verified against every decoded pack buffer and aggregated per
    /// `dir`. Reports ambiguous dirs; `--check` verifies an existing table.
    GenDirindex {
        /// Game data dir holding i.dat and the packs
        #[arg(long = "data-dir", value_name = "DIR")]
        data_dir: PathBuf,
        /// Output path (prints to stdout when omitted)
        #[arg(long)]
        out: Option<PathBuf>,
        /// Verify that --out matches this data dir (update-day check)
        #[arg(long)]
        check: bool,
        /// The game exe (else an ATrain9*.exe next to the data dir is used)
        #[arg(long, value_name = "EXE")]
        exe: Option<PathBuf>,
    },
    /// Build a pack file from raw payloads (fixtures/authoring): a family-B
    /// container (images or csvs), a family-A opaque stream (`a`), or a
    /// family-A `i.dat` index (`idat`) + the pack encryption, byte-verified.
    Mkpack {
        /// Container kind: images (0x200 entries: name=w:h:file), csvs
        /// (0x204 entries: name=file), a (family-A: FILE ... concatenated),
        /// or idat (family-A index: name=dir:len:off ...)
        #[arg(long)]
        container: String,
        /// images: NAME=WIDTH:HEIGHT:FILE ...; csvs: NAME=FILE ...;
        /// a: FILE ...; idat: NAME=DIR:LEN:OFF ...
        entries: Vec<String>,
        /// Output pack file
        #[arg(long)]
        out: PathBuf,
    },
    /// Extract one table out of the game's `data/csvp.bin` for authoring
    /// (byte-exact UTF-16 payload by default; `--utf8` for an editable/text
    /// view of the same rows, `--list` to just name the tables).
    ExtractCsv {
        /// Game dir (the dir holding data/csvp.bin)
        #[arg(long)]
        game: PathBuf,
        /// Table name inside csvp (e.g. "train_form.csv"); required unless --list
        #[arg(long)]
        table: Option<String>,
        /// Output file (written verbatim) — omit to write to stdout
        #[arg(long)]
        out: Option<PathBuf>,
        /// List the tables in the container and exit
        #[arg(long)]
        list: bool,
        /// Convert the table to UTF-8 text (readable/editable; the game's
        /// tables are UTF-16LE — a `mode = "rows"` row patch may be either)
        #[arg(long)]
        utf8: bool,
    },
    /// Apply `mode = "rows"` row-patch files to a shipped csvp table offline
    /// and report the merge (the authoring dry-run; nothing is installed).
    PatchCsv {
        /// Game dir (the dir holding data/csvp.bin)
        #[arg(long)]
        game: PathBuf,
        /// Table name inside csvp (e.g. "train_comment.csv")
        #[arg(long)]
        table: String,
        /// Row-patch file(s), applied in order (UTF-16LE or UTF-8)
        #[arg(long = "patch", value_name = "FILE")]
        patches: Vec<PathBuf>,
        /// Write the merged table here (UTF-16 payload, or a UTF-8 view)
        #[arg(long)]
        out: Option<PathBuf>,
        /// Write a UTF-8 view instead of the raw UTF-16 table bytes
        #[arg(long)]
        utf8: bool,
    },
    /// Extract one shipped i.dat row's raw payload bytes from the pack that
    /// hosts it (e.g. "previewtexture.dir", "form_kh32_f.dds", a mesh) — the
    /// authoring "take a shipped asset apart" command. The host pack is
    /// resolved from the embedded dir index, or by a content scan of the
    /// family-A packs; `--pack` forces it.
    ExtractAsset {
        /// Game dir (holding data/i.dat and the packs), or the data dir itself
        #[arg(long)]
        game: PathBuf,
        /// The i.dat row name (bare, e.g. "previewtexture.dir")
        #[arg(long)]
        name: String,
        /// Output file (written verbatim) — omit to write to stdout
        #[arg(long)]
        out: Option<PathBuf>,
        /// Force the host pack (skip resolution), e.g. "tp"
        #[arg(long)]
        pack: Option<String>,
    },
    /// Build the offline `[[asset]]` merge for one mod: append each payload to
    /// its host pack's decoded stream and synthesize the merged `i.dat`, using
    /// the exact `a9-formats` primitives the loader runs (the authoring proof
    /// that the merged artifacts are correct; nothing is installed).
    MergeAssets {
        /// Game dir (holding data/i.dat and the packs), or the data dir itself
        #[arg(long)]
        game: PathBuf,
        /// The mod dir (holding mod.toml)
        #[arg(long)]
        mod_dir: PathBuf,
        /// Output dir (created): the merged `<host>.bin` pack(s) + `i.dat`
        #[arg(long)]
        out: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();
    let result = match &cli.cmd {
        Cmd::Verify { data_dirs, oracle, expect_rebuild, only } => {
            verify(data_dirs, oracle.as_deref(), expect_rebuild, only)
        }
        Cmd::Inspect { path, all } => inspect(path, *all),
        Cmd::List { game } => list(game),
        Cmd::Doctor { game } => doctor(game),
        Cmd::Classify { path, entry, pack } => {
            classify::run(path.as_deref(), entry.as_deref(), pack.as_deref())
        }
        Cmd::DiffPack {
            pack,
            base,
            modded,
            out,
            id,
            name,
            author,
            game_builds,
        } => diff_pack(pack, base, modded, out, id, name, author, game_builds),
        Cmd::DiffCsv {
            base,
            modded,
            out,
            id,
            name,
            author,
            game_builds,
            rows,
        } => diff_csv(base, modded, out, id, name, author, game_builds, *rows),
        Cmd::GenBuilds { exe, out, check } => gen_builds::run(exe, out.as_deref(), *check),
        Cmd::GenDirindex {
            data_dir,
            out,
            check,
            exe,
        } => gen_dirindex::run(data_dir, exe.as_deref(), out.as_deref(), *check),
        Cmd::Mkpack {
            container,
            entries,
            out,
        } => mkpack(container, entries, out),
        Cmd::ExtractCsv {
            game,
            table,
            out,
            list,
            utf8,
        } => extract_csv(game, table.as_deref(), out.as_deref(), *list, *utf8),
        Cmd::PatchCsv {
            game,
            table,
            patches,
            out,
            utf8,
        } => patch_csv(game, table, patches, out.as_deref(), *utf8),
        Cmd::ExtractAsset {
            game,
            name,
            out,
            pack,
        } => asset::run(game, name, out.as_deref(), pack.as_deref()),
        Cmd::MergeAssets { game, mod_dir, out } => asset::run_merge(game, mod_dir, out),
    };
    if let Err(e) = result {
        eprintln!("a9ml: {e:#}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------- verify

struct Check {
    name: &'static str,
    ok: bool,
    note: String,
}

impl Check {
    fn ok(name: &'static str, note: impl Into<String>) -> Self {
        Self { name, ok: true, note: note.into() }
    }
    fn fail(name: &'static str, note: impl Into<String>) -> Self {
        Self { name, ok: false, note: note.into() }
    }
}

fn verify(
    data_dirs: &[PathBuf],
    oracle: Option<&Path>,
    expect_rebuild: &[String],
    only: &[String],
) -> Result<()> {
    let mut failed = 0usize;

    // --expect-rebuild entries: "KEY" (every swept dir) or "KEY@DIRSUBSTR"
    // (only data dirs whose path contains DIRSUBSTR) — e.g. "csvp@frybarec"
    // scopes the expectation to the Steam install so the *original* JP csvp
    // (a different, ARTDINK-encoded file) cannot false-fail it.
    struct Expect {
        key: String,
        dir: Option<String>,
        seen: bool,
    }
    let mut expects: Vec<Expect> = expect_rebuild
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
        .collect();

    println!("== a9ml verify — self-tests (synthetic)");
    for c in self_tests() {
        report(c.name, c.ok, &c.note);
        failed += !c.ok as usize;
    }

    let oracle = match oracle {
        Some(dir) => {
            let enc = dir.join("a9enc");
            let dec = dir.join("a9dec");
            if !enc.is_file() || !dec.is_file() {
                bail!("--oracle {}: a9enc/a9dec not found there", dir.display());
            }
            println!("== oracle: {} (a9enc + a9dec)", dir.display());
            Some(dir)
        }
        None => {
            println!("== oracle: none (no differential byte-parity checks)");
            None
        }
    };

    for dir in data_dirs {
        println!("== sweeping {}", dir.display());
        let dir_str = dir.to_string_lossy().to_string();
        let mut entries: Vec<PathBuf> = fs::read_dir(dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        entries.sort();
        let mut skipped: Vec<String> = Vec::new();
        for path in &entries {
            let key = fmt::pack::classify_key(
                &path.file_name().unwrap().to_string_lossy(),
            );
            let Some((family, container)) = fmt::pack::classify(
                &path.file_name().unwrap().to_string_lossy(),
            ) else {
                skipped.push(key);
                continue;
            };
            if !only.is_empty() && !only.iter().any(|o| o.eq_ignore_ascii_case(&key)) {
                continue;
            }
            let mut checks: Vec<Check> = Vec::new();
            let file = fs::read(path).context("reading pack")?;
            if file.len() < 24 {
                checks.push(Check::ok("stub", format!("{} bytes — skipped (0-byte stub)", file.len())));
                print_row(&key, &checks);
                continue;
            }
            // decode + integrity
            let decoded = match fmt::pack::decode_pack(&file, family) {
                Ok(d) => d,
                Err(e) => {
                    checks.push(Check::fail("decode", e.to_string()));
                    print_row(&key, &checks);
                    failed += 1;
                    continue;
                }
            };
            let plain = fmt::crypt::decrypt_body(&file).unwrap();
            let declared = if family == fmt::pack::Family::A {
                Some(u32::from_le_bytes(plain[..4].try_into().unwrap()) as usize)
            } else {
                None
            };
            checks.push(Check::ok("decode", format_size_note(decoded.len(), declared)));
            // container invariants + serialize parity
            match container {
                fmt::pack::Container::Images => {
                    match fmt::container::parse_images(&decoded) {
                        Ok(entries) => {
                            let n = entries.len();
                            if fmt::container::serialize_images(&entries) == decoded {
                                checks.push(Check::ok("container", format!("{n} image entries")));
                            } else {
                                checks.push(Check::fail("container", "serialize != decoded"));
                            }
                        }
                        Err(e) => checks.push(Check::fail("container", e.to_string())),
                    }
                }
                fmt::pack::Container::Csvs => {
                    match fmt::container::parse_csvs(&decoded) {
                        Ok(entries) => {
                            let n = entries.len();
                            if fmt::container::serialize_csvs(&entries) == decoded {
                                checks.push(Check::ok("container", format!("{n} csv entries")));
                            } else {
                                checks.push(Check::fail("container", "serialize != decoded"));
                            }
                        }
                        Err(e) => checks.push(Check::fail("container", e.to_string())),
                    }
                }
                fmt::pack::Container::Idat => {
                    match fmt::idat::parse(&decoded) {
                        Ok(idat) => {
                            let n = idat.entries.len();
                            if fmt::idat::serialize(&idat) == decoded {
                                checks.push(Check::ok("container", format!("{n} i.dat rows")));
                            } else {
                                checks.push(Check::fail("container", "serialize != decoded"));
                            }
                        }
                        Err(e) => checks.push(Check::fail("container", e.to_string())),
                    }
                }
                fmt::pack::Container::Opaque => {
                    checks.push(Check::ok("container", "opaque (family-A stream)"));
                }
            }
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
            if let Some(i) = expect_idx {
                expects[i].seen = true;
            }
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
                let od = oracle_decode(oracle, &file, skip, &key)
                    .with_context(|| format!("oracle a9dec failed for {key}"))?;
                if od == decoded {
                    checks.push(Check::ok("oracle-dec", "a9dec byte-identical"));
                } else {
                    checks.push(Check::fail("oracle-dec", "a9dec output differs"));
                }
                let oe = oracle_encode(oracle, &decoded, &key)
                    .with_context(|| format!("oracle a9enc failed for {key}"))?;
                if oe == lz {
                    checks.push(Check::ok("oracle-enc", "a9enc byte-identical"));
                } else {
                    checks.push(Check::fail("oracle-enc", "a9enc output differs"));
                }
            }
            let row_failed = checks.iter().any(|c| !c.ok);
            failed += row_failed as usize;
            print_row(&key, &checks);
        }
        if !skipped.is_empty() {
            println!("   skipped (not classified as packs): {}", skipped.join(", "));
        }
    }

    // --expect-rebuild completeness: every expectation must have matched a
    // swept pack somewhere (typo protection)
    for ex in &expects {
        if !ex.seen {
            println!(
                "   expectation '{}'{} never matched a swept pack",
                ex.key,
                ex.dir.as_deref().map(|d| format!("@{d}")).unwrap_or_default()
            );
            failed += 1;
        }
    }
    if !expects.is_empty() {
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

    let verdict = if failed == 0 { "ALL GREEN".to_string() } else { format!("{failed} FAILURES") };
    println!("== {verdict}");
    if failed > 0 {
        bail!("{failed} check(s) failed");
    }
    Ok(())
}

fn format_size_note(decoded: usize, declared: Option<usize>) -> String {
    match declared {
        Some(d) if d != decoded => format!("{decoded} B decoded (declared {d})"),
        Some(_) => format!("{decoded} B decoded (declared matches)"),
        None => format!("{decoded} B decoded"),
    }
}

fn report(name: &str, ok: bool, note: &str) {
    println!("   [{}] {name}: {note}", if ok { " ok " } else { "FAIL" });
}

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

// ------------------------------------------------------- list / doctor

use a9_mod as amod;

/// Find the game exe in a game dir (Steam name first, then JP names).
fn find_game_exe(game: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = fs::read_dir(game)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension().map(|x| x == "exe").unwrap_or(false)
                && p.file_name()
                    .map(|n| n.to_string_lossy().starts_with("ATrain9"))
                    .unwrap_or(false)
        })
        .collect();
    candidates.sort_by_key(|p| {
        // prefer ATrain9v5.exe (the international Steam exe), then g_x64
        let n = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        (if n == "ATrain9v5.exe" { 0 } else if n == "ATrain9v5g_x64.exe" { 1 } else { 2 }, n)
    });
    candidates.into_iter().next()
}

/// Shared offline resolution report for list/doctor. Nothing is applied —
/// this is the dry view of what the loader does at boot.
fn offline_resolution(game: &Path) -> Result<Offline> {
    let cfg = amod::config::read(&game.join("a9loader.toml"));
    let exe = find_game_exe(game);
    let build = exe
        .as_ref()
        .map(|p| fs::read(p).ok())
        .flatten()
        .and_then(|b| amod::buildid::from_exe_bytes(&b));
    let scanned = amod::scan::scan(&game.join(&cfg.mods_dir));
    let mut manifests = Vec::new();
    let mut dirs = Vec::new();
    let mut errors: Vec<(PathBuf, String)> = Vec::new();
    for s in &scanned {
        match &s.manifest {
            Ok(m) => {
                manifests.push(m.clone());
                dirs.push(s.dir.clone());
            }
            Err(e) => errors.push((s.dir.clone(), e.to_string())),
        }
    }
    let resolution = if manifests.is_empty() {
        None
    } else {
        Some(amod::resolve::resolve(&manifests, &cfg.resolve_config()))
    };
    let out = Offline {
        cfg,
        exe,
        build,
        scanned,
        manifests,
        dirs,
        errors,
        resolution,
    };
    Ok(out)
}

struct Offline {
    cfg: amod::config::Config,
    exe: Option<PathBuf>,
    build: Option<amod::buildid::BuildId>,
    scanned: Vec<amod::scan::ScannedMod>,
    manifests: Vec<amod::manifest::Manifest>,
    /// mod dirs, index-aligned with `manifests` (for offline payload checks)
    dirs: Vec<PathBuf>,
    errors: Vec<(PathBuf, String)>,
    resolution: Option<amod::resolve::Resolution>,
}

/// Best-effort read of the shipped `data/i.dat` row names for the offline
/// `[[asset]]` collision report. `None` = not readable/parseable (the report
/// then says so; the loader's merge-time check stays authoritative).
fn vanilla_asset_names(game: &Path) -> Option<std::collections::BTreeSet<String>> {
    let file = fs::read(game.join("data/i.dat")).ok()?;
    let decoded = fmt::pack::decode_pack(&file, Family::A).ok()?;
    let idat = fmt::idat::parse(&decoded).ok()?;
    Some(idat.entries.iter().map(|e| e.name.clone()).collect())
}

fn print_common(o: &Offline, game: &Path, verbose: bool) {
    println!("game dir: {}", game.display());
    match (&o.exe, &o.build) {
        (Some(exe), Some(b)) => println!(
            "exe: {} — build id {} (FILEVERSION {})",
            exe.display(),
            b.id,
            b.tuple()
        ),
        (Some(exe), None) => println!("exe: {} — no VERSIONINFO found", exe.display()),
        (None, _) => println!("exe: none found (expected ATrain9v5*.exe)"),
    }
    let book = amod::builds::embedded_book().ok();
    let known_build = o
        .build
        .as_ref()
        .map(|b| book.as_ref().map(|bk| bk.build.contains_key(&b.id)).unwrap_or(false));
    if let Some(false) = known_build {
        println!("  this loader build does NOT know that build id — the loader will apply no memory effects");
    }
    println!(
        "config: mods_dir \"{}\", conflicts={}, disabled {:?}",
        o.cfg.mods_dir.display(),
        if o.cfg.allow_conflicts { "warn+load" } else { "error" },
        o.cfg.disabled
    );
    for w in &o.cfg.warnings {
        println!("  config warning: {w}");
    }
    if o.cfg.probe_fetch {
        println!("  probe: fetch hooks on (diagnostic)");
    }
    match o.cfg.pack_serving {
        amod::config::PackServing::Merge => {
            println!("  packs: merge ([[pack]] effects serve via the M3 cache overlay)")
        }
        amod::config::PackServing::Inject => {
            // the injectable set = family-B image packs ([[pack]] sections);
            // per-pack counts, priority order
            let mut inject_packs = std::collections::BTreeMap::<String, usize>::new();
            if let Some(res) = &o.resolution {
                for &i in &res.order {
                    for p in &o.manifests[i].pack {
                        if matches!(
                            pack::classify(&p.pack),
                            Some((Family::B, pack::Container::Images))
                        ) {
                            *inject_packs.entry(p.pack.clone()).or_default() += p.entry.len();
                        }
                    }
                }
            }
            if inject_packs.is_empty() {
                println!("  packs: inject (no family-B [[pack]] effects — nothing to serve)");
            } else {
                let joined = inject_packs
                    .iter()
                    .map(|(p, n)| format!("{p} ({n} entr{})", if *n == 1 { "y" } else { "ies" }))
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("  packs: inject — {joined}");
            }
        }
    }
    let n_mods = o.scanned.len();
    let dir_word = if n_mods == 1 { "directory" } else { "directories" };
    println!(
        "mods dir: {} — {} {}",
        game.join(&o.cfg.mods_dir).display(),
        n_mods,
        dir_word
    );
    for (dir, e) in &o.errors {
        println!("  broken mod {}: {e}", dir.display());
    }
    if let Some(res) = &o.resolution {
        for w in &res.warnings {
            println!("  warning: {w}");
        }
        for s in &res.skipped {
            println!("  skipped '{}': {}", s.id, s.reason);
        }
        for o2 in &res.overlaps {
            let owners: Vec<String> = o2.owners.iter().map(|&i| o.manifests[i].id.clone()).collect();
            println!("  overlap on {}: {} (earlier mod keeps it)", o2.key, owners.join(" vs "));
        }
        let build_id = o.build.as_ref().map(|b| b.id.as_str());
        let vanilla_assets = vanilla_asset_names(game);
        for &i in &res.order {
            let m = &o.manifests[i];
            let gated = build_id.map(|bid| {
                !m.game_builds.is_empty() && !m.game_builds.iter().any(|b| b == bid)
            });
            let gate_note = match gated {
                Some(true) => "  [GATED: game_builds excludes this build — effects will be refused]",
                _ => "",
            };
            println!(
                "  order: '{}' v{} by {} (prio {}) — {} patch, {} hook{gate_note}",
                m.id,
                m.version,
                m.author,
                *o.cfg.priorities.get(&m.id).unwrap_or(&m.priority),
                m.patch.len(),
                m.hook.len()
            );
            // where the game consumes this mod's data effects (additive line —
            // the `order:`/`packs:` lines above stay byte-compatible)
            let classify_rows = classify::classify_manifest(m);
            if classify_rows.is_empty() {
                println!("    classify: (no [[pack]]/[[csv]]/[[asset]]/[[file]] data effects)");
            } else {
                println!(
                    "    classify: {}",
                    classify::Summary::of(&classify_rows).compact()
                );
            }
            // csvp data-effect count — row patches distinctly (M5 W2)
            if !m.csv.is_empty() {
                let n = m.csv.len();
                let rows: usize = m
                    .csv
                    .iter()
                    .filter(|c| c.mode == CsvMode::Rows)
                    .map(|c| c.row_keys.len())
                    .sum();
                let replaces = m.csv.iter().filter(|c| c.mode == CsvMode::Replace).count();
                print!("    csv: {n} csv table{}", if n == 1 { "" } else { "s" });
                let mut parts: Vec<String> = Vec::new();
                if rows > 0 {
                    parts.push(format!(
                        "{rows} row patch{}",
                        if rows == 1 { "" } else { "es" }
                    ));
                }
                if replaces > 0 {
                    parts.push(format!(
                        "{replaces} whole-table replace{}",
                        if replaces == 1 { "" } else { "s" }
                    ));
                }
                if parts.is_empty() {
                    println!();
                } else {
                    println!(" ({})", parts.join(", "));
                }
            }
            // [[asset]] roll-up (M5 W1 / W1b) — new rows vs overrides of
            // shipped ones, and the `host = "auto"` resolution
            let mut auto_hosts: std::collections::BTreeMap<
                String,
                Result<String, String>,
            > = std::collections::BTreeMap::new();
            if !m.asset.is_empty() {
                let n = m.asset.len();
                // resolve every `host = "auto"` row against the embedded dir
                // index for the running build (the same call the loader makes)
                let autos: Vec<&amod::manifest::AssetSection> = m
                    .asset
                    .iter()
                    .filter(|a| a.host.eq_ignore_ascii_case(amod::manifest::HOST_AUTO))
                    .collect();
                if !autos.is_empty() {
                    let dirindex = amod::dirindex::embedded_index().ok();
                    for a in &autos {
                        let r = match (o.build.as_ref(), &dirindex) {
                            (Some(b), Some(idx)) => {
                                amod::dirindex::resolve_auto(idx, &b.id, &a.name, &a.dir)
                            }
                            (None, _) => Err(
                                "unknown build (no exe identity) — cannot resolve host = \"auto\""
                                    .to_string(),
                            ),
                            (_, None) => {
                                Err("the embedded dir index failed to parse".to_string())
                            }
                        };
                        auto_hosts.insert(a.name.clone(), r);
                    }
                    let ok = auto_hosts.values().filter(|r| r.is_ok()).count();
                    let refused = auto_hosts.len() - ok;
                    let mut hosts: Vec<String> = auto_hosts
                        .values()
                        .filter_map(|r| r.as_ref().ok().cloned())
                        .collect();
                    hosts.sort();
                    hosts.dedup();
                    println!(
                        "    host auto: {ok} resolved{}; {refused} refused",
                        if hosts.is_empty() {
                            String::new()
                        } else {
                            format!(" ({})", hosts.join(", "))
                        }
                    );
                    for (name, r) in &auto_hosts {
                        if let Err(e) = r {
                            println!("      refusal: {name}: {e}");
                        }
                    }
                }
                match &vanilla_assets {
                    Some(names) => {
                        let over = m.asset.iter().filter(|a| names.contains(&a.name)).count();
                        println!(
                            "    assets: {n} ({} new, {over} vanilla override{})",
                            n - over,
                            if over == 1 { "" } else { "s" }
                        );
                        let need: Vec<&str> = m
                            .asset
                            .iter()
                            .filter(|a| names.contains(&a.name) && !a.override_vanilla)
                            .map(|a| a.name.as_str())
                            .collect();
                        if !need.is_empty() {
                            println!(
                                "      warning: {} asset name(s) collide with the shipped i.dat \
                                 and need `override_vanilla = true`: {}",
                                need.len(),
                                need.join(", ")
                            );
                        }
                    }
                    None => println!(
                        "    assets: {n} (vanilla data/i.dat not readable — collisions unchecked \
                         offline; the loader refuses them at merge time)"
                    ),
                }
                // the payload must exist in the mod dir (offline check; the
                // loader also fails loudly at merge time)
                let missing: Vec<&str> = m
                    .asset
                    .iter()
                    .filter(|a| o.dirs.get(i).is_some_and(|d| !d.join(&a.file).is_file()))
                    .map(|a| a.file.as_str())
                    .collect();
                if !missing.is_empty() {
                    println!(
                        "      warning: {} asset payload file(s) missing in the mod dir: {}",
                        missing.len(),
                        missing.join(", ")
                    );
                }
            }
            if verbose {
                for p in &m.patch {
                    println!("    [[patch]] {} off={} len={}", p.target.key(p.offset), p.offset, amod::hex::parse(&p.data).map(|d| d.len()).unwrap_or(0));
                }
                for h in &m.hook {
                    println!("    [[hook]] {} handler={} list={}", h.target.key(0), h.handler, h.list.as_deref().unwrap_or("-"));
                }
                for p in &m.pack {
                    for e in &p.entry {
                        println!("    [[pack]] {} entry {} ({}x{}, {})", p.pack, e.name, e.width, e.height, e.file);
                    }
                }
                for c in &m.csv {
                    println!(
                        "    [[csv]] {} ({}, {})",
                        c.name,
                        c.file,
                        match c.mode {
                            CsvMode::Replace => "replace",
                            CsvMode::Rows => "rows",
                        }
                    );
                }
                for f in &m.file {
                    println!("    [[file]] {} <- {}", f.dest, f.file);
                }
                for a in &m.asset {
                    let host = match auto_hosts.get(&a.name) {
                        Some(Ok(h)) => format!("{h} [auto]"),
                        Some(Err(_)) => "auto [REFUSED]".to_string(),
                        None => a.host.clone(),
                    };
                    println!(
                        "    [[asset]] {} (host {host}, dir {}, file {}{})",
                        a.name,
                        a.dir,
                        a.file,
                        if a.override_vanilla { ", override_vanilla" } else { "" }
                    );
                }
                for r in &classify_rows {
                    println!("      {}", r.line());
                }
            }
        }
        if verbose {
            println!("  (offline resolution — the loader applies this at boot)");
        }
    } else if n_mods == 0 {
        println!("  (no mods — the loader is a pure proxy)");
    }
}

fn list(game: &Path) -> Result<()> {
    if !game.is_dir() {
        bail!("not a directory: {}", game.display());
    }
    let o = offline_resolution(game)?;
    print_common(&o, game, true);
    Ok(())
}

fn doctor(game: &Path) -> Result<()> {
    if !game.is_dir() {
        bail!("not a directory: {}", game.display());
    }
    let o = offline_resolution(game)?;
    print_common(&o, game, false);
    // loader dll presence
    let dll = game.join("version.dll");
    println!(
        "loader: {}",
        if dll.is_file() {
            "version.dll present"
        } else {
            "version.dll MISSING — the game runs vanilla"
        }
    );
    // log tail
    let log = game.join(&o.cfg.mods_dir).join("a9loader.log");
    if log.is_file() {
        println!("---- a9loader.log (last 30 lines) ----");
        let text = fs::read_to_string(&log).context("reading the loader log")?;
        for l in text.lines().rev().take(30).collect::<Vec<_>>().iter().rev() {
            println!("{l}");
        }
    } else {
        println!("loader log: none at {} (no boot with the M2+ loader yet?)", log.display());
    }
    Ok(())
}

// ------------------------------------------------------ diff / mkpack

use a9_formats::container::{parse_csvs, parse_images, CsvEntry, ImageEntry};
use a9_formats::csv;
use a9_formats::pack::{self, Family};
use a9_mod::manifest::{CsvMode, CsvSection, Manifest, PackEntry, PackSection};

fn sanitized(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect()
}

fn manifest_toml(m: &Manifest) -> Result<String> {
    toml::to_string(m).context("serializing mod.toml")
}

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
        asset: Vec::new(),
        file: Vec::new(),
    };
    m.validate().map_err(|e| anyhow::anyhow!("generated manifest invalid: {e}"))?;
    fs::create_dir_all(out).context("creating the mod dir")?;
    fs::write(out.join("mod.toml"), manifest_toml(&m)?).context("writing mod.toml")?;
    Ok(())
}

fn diff_pack(
    pack: &str,
    base: &Path,
    modded: &Path,
    out: &Path,
    id: &str,
    name: &str,
    author: &str,
    game_builds: &[String],
) -> Result<()> {
    let base_f = fs::read(&base).with_context(|| format!("reading {}", base.display()))?;
    let mod_f = fs::read(&modded).with_context(|| format!("reading {}", modded.display()))?;
    let base_e = parse_images(&pack::decode_pack(&base_f, Family::B)?)
        .context("parsing the base pack (family-B 0x200 expected)")?;
    let mod_e = parse_images(&pack::decode_pack(&mod_f, Family::B)?)
        .context("parsing the modded pack (family-B 0x200 expected)")?;

    let base_map: std::collections::BTreeMap<String, &ImageEntry> =
        base_e.iter().map(|e| (e.name.as_str(), e)).collect();
    let mod_names: std::collections::BTreeSet<String> =
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
        let n = e.name.as_str();
        let existing = base_map.get(&n);
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
        let file = format!("entries/{}", sanitized(&n));
        fs::write(out.join(&file), &e.payload)
            .with_context(|| format!("writing {}", out.join(&file).display()))?;
        total_bytes += e.payload.len();
        section.entry.push(PackEntry {
            name: n.clone(),
            width: e.width,
            height: e.height,
            file,
        });
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

fn diff_csv(
    base: &Path,
    modded: &Path,
    out: &Path,
    id: &str,
    name: &str,
    author: &str,
    game_builds: &[String],
    rows_mode: bool,
) -> Result<()> {
    let base_f = fs::read(&base).with_context(|| format!("reading {}", base.display()))?;
    let mod_f = fs::read(&modded).with_context(|| format!("reading {}", modded.display()))?;
    let base_e = parse_csvs(&pack::decode_pack(&base_f, Family::B)?)
        .context("parsing the base csvp (0x204 container expected)")?;
    let mod_e = parse_csvs(&pack::decode_pack(&mod_f, Family::B)?)
        .context("parsing the modded csvp (0x204 container expected)")?;
    let base_map: std::collections::BTreeMap<String, &CsvEntry> =
        base_e.iter().map(|e| (e.name.as_str(), e)).collect();
    let mod_names: std::collections::BTreeSet<String> =
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
        let n = e.name.as_str();
        let existing = base_map.get(&n);
        let changed = existing.map_or(true, |b| b.payload != e.payload);
        if !changed {
            continue;
        }
        if existing.is_some() {
            replaced += 1;
        } else {
            added += 1;
        }
        // row-level view of the change (best effort — reports nothing when a
        // side is not a parseable UTF-16 table)
        let (r_replaced, r_added, r_removed) = match existing {
            Some(b) => diff_rows(&b.payload, &e.payload),
            None => (Vec::new(), Vec::new(), Vec::new()),
        };
        let file = format!("csv/{}", sanitized(&n));
        // A pure row-level change (existing table, parseable, nothing removed)
        // can ship as a compact row patch under --rows.
        let as_rows = rows_mode
            && existing.is_some()
            && r_removed.is_empty()
            && !(r_replaced.is_empty() && r_added.is_empty());
        if as_rows {
            let modded_tbl = csv::Table::parse(&e.payload)
                .with_context(|| format!("parsing changed table {n} (row mode)"))?;
            let keys: std::collections::BTreeSet<String> = r_replaced
                .iter()
                .chain(r_added.iter())
                .cloned()
                .collect();
            let patch = row_patch_bytes(&modded_tbl, &keys);
            fs::write(out.join(&file), &patch)
                .with_context(|| format!("writing {}", out.join(&file).display()))?;
            sections.push(CsvSection {
                name: n.to_string(),
                file,
                mode: CsvMode::Rows,
                row_keys: Vec::new(),
            });
            println!(
                "    {n}: row patch — {} row(s) replaced, {} added{}",
                r_replaced.len(),
                r_added.len(),
                list_keys(&r_replaced, &r_added)
            );
        } else {
            fs::write(out.join(&file), &e.payload)
                .with_context(|| format!("writing {}", out.join(&file).display()))?;
            sections.push(CsvSection {
                name: n.to_string(),
                file,
                mode: CsvMode::Replace,
                row_keys: Vec::new(),
            });
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
        }
        row_replaced_total += r_replaced.len();
        row_added_total += r_added.len();
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

/// Row-level comparison of two table payloads: (replaced keys, added keys,
/// removed keys), each sorted. Empty when a side is not a parseable table or
/// has no keyed rows.
fn diff_rows(base: &[u8], modded: &[u8]) -> (Vec<String>, Vec<String>, Vec<String>) {
    let (Ok(b), Ok(m)) = (csv::Table::parse(base), csv::Table::parse(modded)) else {
        return (Vec::new(), Vec::new(), Vec::new());
    };
    let bmap: std::collections::BTreeMap<&str, &csv::Row> = b
        .rows
        .iter()
        .filter(|r| !r.key().is_empty())
        .map(|r| (r.key(), r))
        .collect();
    let mmap: std::collections::BTreeMap<&str, &csv::Row> = m
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
fn row_patch_bytes(modded: &csv::Table, keys: &std::collections::BTreeSet<String>) -> Vec<u8> {
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

fn mkpack(container: &str, entries: &[String], out: &Path) -> Result<()> {
    let (file, family) = match container {
        "images" => {
            let mut ents = Vec::new();
            for spec in entries {
                // NAME=WIDTH:HEIGHT:FILE (split from the right: the dims
                // themselves contain a colon)
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
            (
                pack::encode_pack_b(&a9_formats::container::serialize_images(&ents)),
                Family::B,
            )
        }
        "csvs" => {
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
            (
                pack::encode_pack_b(&a9_formats::container::serialize_csvs(&ents)),
                Family::B,
            )
        }
        // a family-A opaque stream: the named files' bytes, concatenated (the
        // M5 W1 fixture path — a host pack `V`)
        "a" => {
            let mut decoded = Vec::new();
            for path in entries {
                decoded.extend_from_slice(&fs::read(path).with_context(|| format!("reading {path}"))?);
            }
            (pack::encode_pack_a(&decoded), Family::A)
        }
        // a family-A `i.dat` index: NAME=DIR:LEN:OFF (OFF optional, default 0);
        // tails are synthesized clean (flag 0), pad = the Steam build's
        "idat" => {
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
            (pack::encode_pack_a(&fmt::idat::serialize(&idat)), Family::A)
        }
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

// ---------------------------------------------------------------- extract-csv

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

fn extract_csv(
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

fn patch_csv(
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

fn inspect(path: &Path, all: bool) -> Result<()> {
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    let key = fmt::pack::classify_key(&name);
    let Some((family, container)) = fmt::pack::classify(&name) else {
        bail!("{name}: not a recognized pack (classify -> None)");
    };
    let file = fs::read(path)?;
    let decoded = fmt::pack::decode_pack(&file, family)
        .with_context(|| format!("{name}: decode failed"))?;
    let plain = fmt::crypt::decrypt_body(&file)?;
    println!("{key}: family {:?}", family);
    println!("  file {} B, decoded {} B", file.len(), decoded.len());
    if family == fmt::pack::Family::A {
        let declared = u32::from_le_bytes(plain[..4].try_into().unwrap());
        println!("  declared output {} B (decoded {} B)", declared, decoded.len());
    }
    let cap = if all { usize::MAX } else { 40 };
    match container {
        fmt::pack::Container::Images => {
            let entries = fmt::container::parse_images(&decoded)?;
            println!("  {} image entries:", entries.len());
            for e in entries.iter().take(cap) {
                println!("    {:<28} {}x{} {:>10} B", e.name.as_str(), e.width, e.height, e.payload.len());
            }
            if entries.len() > cap {
                println!("    … {} more", entries.len() - cap);
            }
        }
        fmt::pack::Container::Csvs => {
            let entries = fmt::container::parse_csvs(&decoded)?;
            println!("  {} csv entries:", entries.len());
            for e in entries.iter().take(40) {
                println!("    {:<28} {:>10} B", e.name.as_str(), e.payload.len());
            }
            if entries.len() > 40 {
                println!("    … {} more", entries.len() - 40);
            }
        }
        fmt::pack::Container::Idat => {
            let idat = fmt::idat::parse(&decoded)?;
            let dirs: std::collections::BTreeMap<String, usize> =
                idat.entries.iter().fold(std::collections::BTreeMap::new(), |mut m, e| {
                    *m.entry(e.dir.clone()).or_default() += 1;
                    m
                });
            println!(
                "  {} i.dat rows, {} dirs (header pad: {})",
                idat.entries.len(),
                dirs.len(),
                idat.pad.iter().map(|b| format!("{b:02x}")).collect::<String>()
            );
            for (d, n) in dirs.iter().take(40) {
                println!("    {n:>7}  {d}");
            }
            if all {
                // per-row view: bare name, dir, and the (len, flag, off) tail —
                // the row fields a [[asset]] merge writes (M5 W1)
                for e in idat.entries.iter() {
                    println!(
                        "    {:<28} {:<18} len={:<9} flag={} off={}",
                        e.name,
                        e.dir,
                        e.v1(),
                        e.v2(),
                        e.v3()
                    );
                }
            }
        }
        fmt::pack::Container::Opaque => println!("  opaque family-A stream"),
    }
    Ok(())
}

// ---------------------------------------------------------------- self-tests

fn self_tests() -> Vec<Check> {
    let mut out = Vec::new();
    // cipher round-trip + header
    {
        let plain: Vec<u8> = (0..50_000u32).map(|i| (i * 2654435761 >> 24) as u8).collect();
        let file = fmt::crypt::encrypt_file(&plain);
        out.push(if fmt::crypt::header_matches(&file, &plain)
            && fmt::crypt::decrypt_body(&file).map(|d| d == plain).unwrap_or(false)
        {
            Check::ok("crypt", "encrypt/decrypt + md5 header")
        } else {
            Check::fail("crypt", "round-trip mismatch")
        });
    }
    // lzss round-trip incl. overlap
    {
        let mut data = b"abcabcabcabc".to_vec();
        data.extend_from_slice(&vec![0u8; 8192]);
        data.extend_from_slice(b"abcabcabc tail");
        out.push(if fmt::lzss::decode(&fmt::lzss::encode(&data), 0) == data {
            Check::ok("lzss", "encode/decode round-trip (overlap + wrap)")
        } else {
            Check::fail("lzss", "round-trip mismatch")
        });
    }
    // image + csv container parity (verbatim name fields, incl. garbage padding)
    {
        let imgs = vec![fmt::container::ImageEntry {
            name: fmt::container::NameField::from_str("_d_test_a.dds").unwrap(),
            width: 8,
            height: 4,
            payload: vec![9u8; 33],
        }];
        let ser = fmt::container::serialize_images(&imgs);
        out.push(
            if fmt::container::parse_images(&ser).map(|p| p == imgs).unwrap_or(false)
                && fmt::container::serialize_images(&fmt::container::parse_images(&ser).unwrap()) == ser
            {
                Check::ok("container-images", "parse/serialize byte parity")
            } else {
                Check::fail("container-images", "mismatch")
            },
        );
        let csvs = vec![fmt::container::CsvEntry {
            name: fmt::container::NameField::from_str("train_comment.csv").unwrap(),
            payload: b"a,b\r\n".to_vec(),
        }];
        let ser = fmt::container::serialize_csvs(&csvs);
        out.push(
            if fmt::container::parse_csvs(&ser).map(|p| p == csvs).unwrap_or(false)
                && fmt::container::serialize_csvs(&fmt::container::parse_csvs(&ser).unwrap()) == ser
            {
                Check::ok("container-csvs", "parse/serialize byte parity")
            } else {
                Check::fail("container-csvs", "mismatch")
            },
        );
    }
    // i.dat parity (both known build paddings)
    {
        let rows = vec![fmt::idat::Entry {
            name: "s_orm_02_f.dds".into(),
            dir: "dai".into(),
            tail: [1, 0, 0, 0, 0, 9, 0, 0, 0],
        }];
        let mut ok = true;
        for pad in [fmt::idat::PAD_DEMO, fmt::idat::PAD_STEAM] {
            let x = fmt::idat::Idat { pad, entries: rows.clone() };
            let ser = fmt::idat::serialize(&x);
            if let Ok(reparsed) = fmt::idat::parse(&ser) {
                ok &= reparsed == x && fmt::idat::serialize(&reparsed) == ser;
            } else {
                ok = false;
            }
        }
        out.push(if ok {
            Check::ok("idat", "parse/serialize byte parity (demo + steam pads)")
        } else {
            Check::fail("idat", "mismatch")
        });
    }
    // full synthetic pack pipeline: family B file -> decode -> container
    {
        let imgs = vec![fmt::container::ImageEntry {
            name: fmt::container::NameField::from_str("_d_pipeline.dds").unwrap(),
            width: 2,
            height: 2,
            payload: vec![7u8; 16],
        }];
        let ser = fmt::container::serialize_images(&imgs);
        let file = fmt::pack::encode_pack_b(&ser);
        let dec = fmt::pack::decode_pack(&file, fmt::pack::Family::B).unwrap();
        out.push(if fmt::container::parse_images(&dec).map(|p| p == imgs).unwrap_or(false) {
            Check::ok("pack-pipeline", "synthetic family-B file round-trip")
        } else {
            Check::fail("pack-pipeline", "mismatch")
        });
    }
    // savegame carving
    {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write as _;
        let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
        e.write_all(b"savegame member").unwrap();
        let z = e.finish().unwrap();
        let members = fmt::savegame::carve_members(&z);
        out.push(if members == vec![b"savegame member".to_vec()] {
            Check::ok("savegame", "zlib member carving")
        } else {
            Check::fail("savegame", "mismatch")
        });
    }
    out
}
