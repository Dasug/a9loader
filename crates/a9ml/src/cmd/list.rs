//! `a9ml list` and `a9ml doctor` — the two offline game-dir views.

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;

use super::report::{offline_resolution, print_common};

pub fn list(game: &Path) -> Result<()> {
    if !game.is_dir() {
        bail!("not a directory: {}", game.display());
    }
    let o = offline_resolution(game)?;
    print_common(&o, game, true);
    Ok(())
}

pub fn doctor(game: &Path) -> Result<()> {
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
        println!("loader log: none at {} (no boot with this loader yet?)", log.display());
    }
    Ok(())
}
