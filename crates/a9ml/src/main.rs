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

mod classify;
mod gen_builds;

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
    /// Build a pack file from raw payloads (fixtures/authoring): a family-B
    /// container (images or csvs) + the pack encryption, byte-verified.
    Mkpack {
        /// Container kind: images (0x200 entries: name=w:h:file) or csvs
        /// (0x204 entries: name=file)
        #[arg(long)]
        container: String,
        /// images: NAME=WIDTH:HEIGHT:FILE ...; csvs: NAME=FILE ...
        entries: Vec<String>,
        /// Output pack file
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
        } => diff_csv(base, modded, out, id, name, author, game_builds),
        Cmd::GenBuilds { exe, out, check } => gen_builds::run(exe, out.as_deref(), *check),
        Cmd::Mkpack {
            container,
            entries,
            out,
        } => mkpack(container, entries, out),
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
    let mut errors: Vec<(PathBuf, String)> = Vec::new();
    for s in &scanned {
        match &s.manifest {
            Ok(m) => manifests.push(m.clone()),
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
    errors: Vec<(PathBuf, String)>,
    resolution: Option<amod::resolve::Resolution>,
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
                println!("    classify: (no [[pack]]/[[csv]]/[[file]] data effects)");
            } else {
                println!(
                    "    classify: {}",
                    classify::Summary::of(&classify_rows).compact()
                );
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
                    println!("    [[csv]] {} ({})", c.name, c.file);
                }
                for f in &m.file {
                    println!("    [[file]] {} <- {}", f.dest, f.file);
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
use a9_formats::pack::{self, Family};
use a9_mod::manifest::{CsvSection, Manifest, PackEntry, PackSection};

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
        let file = format!("csv/{}", sanitized(&n));
        fs::write(out.join(&file), &e.payload)
            .with_context(|| format!("writing {}", out.join(&file).display()))?;
        sections.push(CsvSection {
            name: n.clone(),
            file,
        });
    }
    if sections.is_empty() {
        bail!("no differences found between the csvp files — nothing to generate");
    }
    let n_tables = sections.len();
    write_mod(out, id, name, author, game_builds, Vec::new(), sections)?;
    println!(
        "mod '{id}' generated: csvp — {added} added, {replaced} replaced, {n_tables} tables -> {}",
        out.display()
    );
    Ok(())
}

fn mkpack(container: &str, entries: &[String], out: &Path) -> Result<()> {
    let file = match container {
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
            pack::encode_pack_b(&a9_formats::container::serialize_images(&ents))
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
            pack::encode_pack_b(&a9_formats::container::serialize_csvs(&ents))
        }
        other => bail!("--container must be images or csvs, got {other:?}"),
    };
    // byte-verify the result before writing
    let decoded = pack::decode_pack(&file, Family::B)?;
    let n = match container {
        "images" => parse_images(&decoded)?.len(),
        _ => parse_csvs(&decoded)?.len(),
    };
    anyhow::ensure!(n == entries.len(), "round-trip count mismatch: {n} != {}", entries.len());
    fs::write(out, &file).with_context(|| format!("writing {}", out.display()))?;
    println!(
        "{} written: {} entries, {:.1} MB decoded",
        out.display(),
        entries.len(),
        decoded.len() as f64 / (1024.0 * 1024.0)
    );
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
