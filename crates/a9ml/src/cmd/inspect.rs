//! `a9ml inspect` — decode one pack file and describe it (family, sizes,
//! container, and the first 40 entries unless `--all`).

use a9_formats as fmt;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;

pub fn run(path: &Path, all: bool) -> Result<()> {
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
                // the row fields an [[asset]] merge writes
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
