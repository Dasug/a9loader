//! a9ml — A-Train 9 mod loader authoring & diagnostics CLI.
//!
//! M1 scope: `verify` (format-library proofs against real packs) and
//! `inspect` (decode + describe one pack). Installing a mod stays a folder
//! drop — there is deliberately no install command.
//!
//! This file holds only the CLI surface: the clap argument model ([`Cli`] /
//! [`Cmd`]) and the dispatch to one entry point per subcommand. The
//! subcommand implementations live in [`cmd`], and the two data-model modules
//! ([`asset`], [`classify`]) plus the two generators ([`gen_builds`],
//! [`gen_dirindex`]) keep their own entry points.
//!
//! The argument definitions stay here, together, on purpose: the subcommand
//! names, flags, defaults and help text are the user-visible contract, so
//! they are not scattered across the implementation modules.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod asset;
mod classify;
mod cmd;
mod gen_builds;
mod gen_dirindex;
mod util;

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
    /// Show what each `[[train]]` will allocate and expand to, and validate it
    /// offline (the authoring dry run; nothing is applied). The loader
    /// allocates the spec-row id + FORM name, derives the four csvp rows, the
    /// preview icons and the visibility entry — this prints exactly that.
    Trains {
        /// Game dir (holding a9loader.toml + the mods dir) — resolves the load
        /// order and compiles every mod's trains
        #[arg(long)]
        game: Option<PathBuf>,
        /// A single mod dir (holding mod.toml) — compile just this mod
        #[arg(long = "mod", value_name = "DIR")]
        mod_dir: Option<PathBuf>,
    },
}

fn main() {
    let cli = Cli::parse();
    let result = match &cli.cmd {
        Cmd::Verify { data_dirs, oracle, expect_rebuild, only } => {
            cmd::verify::run(data_dirs, oracle.as_deref(), expect_rebuild, only)
        }
        Cmd::Inspect { path, all } => cmd::inspect::run(path, *all),
        Cmd::List { game } => cmd::list::list(game),
        Cmd::Doctor { game } => cmd::list::doctor(game),
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
        } => cmd::diff::diff_pack(pack, base, modded, out, id, name, author, game_builds),
        Cmd::DiffCsv {
            base,
            modded,
            out,
            id,
            name,
            author,
            game_builds,
            rows,
        } => cmd::diff::diff_csv(base, modded, out, id, name, author, game_builds, *rows),
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
        } => cmd::mkpack::run(container, entries, out),
        Cmd::ExtractCsv {
            game,
            table,
            out,
            list,
            utf8,
        } => cmd::csv::extract(game, table.as_deref(), out.as_deref(), *list, *utf8),
        Cmd::PatchCsv {
            game,
            table,
            patches,
            out,
            utf8,
        } => cmd::csv::patch(game, table, patches, out.as_deref(), *utf8),
        Cmd::ExtractAsset {
            game,
            name,
            out,
            pack,
        } => asset::run(game, name, out.as_deref(), pack.as_deref()),
        Cmd::MergeAssets { game, mod_dir, out } => asset::run_merge(game, mod_dir, out),
        Cmd::Trains { game, mod_dir } => {
            cmd::trains::run(game.as_deref(), mod_dir.as_deref())
        }
    };
    if let Err(e) = result {
        eprintln!("a9ml: {e:#}");
        std::process::exit(1);
    }
}
