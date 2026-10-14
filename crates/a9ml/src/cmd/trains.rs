//! `a9ml trains` — the offline `[[train]]` dry run (M7-2): show what each
//! declared train will allocate and expand to, and validate it without booting
//! the game. Nothing is applied; this is the authoring view of exactly what the
//! loader does at boot.
//!
//! Two shapes, matching the other commands:
//! * `--game <dir>`: scan the game's mods dir, resolve the load order, compile
//!   every `[[train]]` (ids threaded across mods, exactly like the boot), and
//!   report the allocation, the four derived rows, the icons, the effect keys
//!   and the offline checks (a FORM-name collision with the shipped catalog, a
//!   missing icon payload).
//! * `--mod <dir>`: compile just that one mod (ids from the boot base).

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use a9_mod::manifest::Manifest;
use a9_mod::train::{self, CompiledTrain, BASE_ID};

use super::report::find_game_exe;
use crate::gen_builds::rva_to_file;

pub fn run(game: Option<&Path>, mod_dir: Option<&Path>) -> Result<()> {
    let (trains, refusals, forms) = match (game, mod_dir) {
        (Some(game), _) => {
            if !game.is_dir() {
                bail!("not a directory: {}", game.display());
            }
            let cfg = a9_mod::config::read(&game.join("a9loader.toml"));
            let scanned = a9_mod::scan::scan(&game.join(&cfg.mods_dir));
            let mut manifests: Vec<Manifest> = Vec::new();
            let mut dirs: Vec<PathBuf> = Vec::new();
            for s in &scanned {
                match &s.manifest {
                    Ok(m) => {
                        manifests.push(m.clone());
                        dirs.push(s.dir.clone());
                    }
                    Err(e) => println!("  broken mod {}: {e}", s.dir.display()),
                }
            }
            let order: Vec<usize> = if manifests.is_empty() {
                Vec::new()
            } else {
                a9_mod::resolve::resolve(&manifests, &cfg.resolve_config()).order
            };
            let (t, r) = train::compile_all(&manifests, &order);
            let forms = read_shipped_forms(game);
            (t, r, forms)
        }
        (None, Some(m)) => {
            let text = fs::read_to_string(m.join("mod.toml"))
                .with_context(|| format!("reading {}", m.join("mod.toml").display()))?;
            let manifest = Manifest::parse(&text)?;
            manifest.validate()?;
            let (t, r) = train::compile(&manifest.id, &manifest.train, BASE_ID);
            let forms = m
                .parent()
                .and_then(|p| p.parent())
                .map(read_shipped_forms)
                .unwrap_or(None);
            (t, r, forms)
        }
        (None, None) => bail!("give --game <dir> (a mods dir) or --mod <dir>"),
    };

    // the mod dirs, for the icon payload checks
    let dirs: Vec<(String, PathBuf)> = match (game, mod_dir) {
        (_, Some(m)) => vec![(
            read_mod_id(m).unwrap_or_else(|| "<mod>".into()),
            m.to_path_buf(),
        )],
        (Some(game), None) => {
            let cfg = a9_mod::config::read(&game.join("a9loader.toml"));
            a9_mod::scan::scan(&game.join(&cfg.mods_dir))
                .into_iter()
                .filter_map(|s| {
                    s.manifest.ok().map(|m| (m.id, s.dir))
                })
                .collect()
        }
        _ => Vec::new(),
    };

    if trains.is_empty() && refusals.is_empty() {
        println!("trains: none declared");
        return Ok(());
    }
    let collisions = forms
        .as_ref()
        .map(|f| {
            trains
                .iter()
                .filter(|t| f.iter().any(|n| n == &t.form_name))
                .count()
        })
        .unwrap_or(0);
    println!(
        "trains: {} compiled, {} refused, {} boot-collision(s) with the shipped catalog",
        trains.len(),
        refusals.len(),
        collisions
    );
    for t in &trains {
        print_train(t, &forms, &dirs);
    }
    for r in &refusals {
        println!("  refusal: {r}");
    }
    if forms.is_none() {
        println!("  (no game exe: FORM-name collisions unchecked)");
    }
    Ok(())
}

/// The mod id of a `--mod` dir.
fn read_mod_id(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join("mod.toml")).ok()?;
    Manifest::parse(&text).ok().map(|m| m.id)
}

fn print_train(t: &CompiledTrain, forms: &Option<Vec<String>>, dirs: &[(String, PathBuf)]) {
    println!(
        "  [[train]] {} -> {} (spec row id {}, donor {}, {} car(s), cruise {}, fare {}, livery {})",
        t.key, t.form_name, t.id, t.donor, t.cars, t.cruise_speed, t.fare, t.livery
    );
    // the FORM-name collision check (the loader refuses these at boot)
    if let Some(forms) = forms {
        if forms.iter().any(|f| f == &t.form_name) {
            println!(
                "      REFUSED at boot: {} is a SHIPPED catalog name (aliases a real train)",
                t.form_name
            );
        }
    }
    // the four derived rows
    println!(
        "      train_form    {} ({} cols)",
        t.rows.form.join(" | "),
        t.rows.form.len()
    );
    let keyless = t.rows.pattern[1..t.rows.pattern.len() - 1]
        .iter()
        .filter(|r| r.first().map(|s| s.is_empty()).unwrap_or(true))
        .count();
    println!(
        "      train_pattern {} rows (head keyed {}, {} keyless continuation, separator) — \
         positional group append",
        t.rows.pattern.len(),
        t.rows.pattern[0][0],
        keyless
    );
    println!("      train_comment {}", t.rows.comment.join(" | "));
    println!("      train_custom  {}", t.rows.custom.join(" | "));
    // icons + payload presence
    for icon in &t.icons {
        let present = dirs
            .iter()
            .find(|(id, _)| id == &t.mod_id)
            .map(|(_, d)| d.join(&icon.file).is_file());
        println!(
            "      icon {:<34} <- {:<40} {}",
            icon.name,
            icon.file,
            match present {
                Some(true) => "present",
                Some(false) => "MISSING (icon stays blank)",
                None => "unchecked",
            }
        );
    }
    println!("      keys: {}", t.effect_keys().join(", "));
}

/// Read the shipped FORM catalog out of a game exe (offline, via the section
/// table). `None` = the exe/anchor could not be read (the check is skipped).
fn read_shipped_forms(game: &Path) -> Option<Vec<String>> {
    let exe = find_game_exe(game)?;
    let bytes = fs::read(&exe).ok()?;
    let build = a9_mod::buildid::from_exe_bytes(&bytes)?;
    let book = a9_mod::builds::embedded_book().ok()?;
    let anchor = book.build.get(&build.id)?.anchor("form_list")?;
    let (off, _) = rva_to_file(&bytes, anchor.rva).ok()?;
    let mut units: Vec<u16> = Vec::new();
    let mut i = off;
    while i + 1 < bytes.len() && units.len() < 16384 {
        let u = u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        if u == 0 {
            break;
        }
        units.push(u);
        i += 2;
    }
    Some(parse_forms(&units))
}

/// Parse a UTF-16 comma-separated, NUL-terminated FORM list (entries trimmed of
/// spaces/tabs) — the same shape `a9-loader::forms` parses, kept local so
/// `a9ml` needs no loader dependency.
fn parse_forms(units: &[u16]) -> Vec<String> {
    let mut names = Vec::new();
    let mut start = 0usize;
    let mut push = |raw: &[u16]| {
        let mut a = 0usize;
        let mut b = raw.len();
        while a < b && matches!(raw[a], 0x20 | 0x09) {
            a += 1;
        }
        while b > a && matches!(raw[b - 1], 0x20 | 0x09) {
            b -= 1;
        }
        names.push(String::from_utf16_lossy(&raw[a..b]));
    };
    for (i, &u) in units.iter().enumerate() {
        if u == u16::from(b',') {
            push(&units[start..i]);
            start = i + 1;
        }
    }
    push(&units[start..]);
    names
}
