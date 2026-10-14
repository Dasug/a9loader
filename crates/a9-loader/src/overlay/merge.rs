//! The overlay's merge side — cache building and the pack/i.dat merges.
//!
//! `ensure_cache` is the hook-facing entry: it builds the merged pack at most
//! once per run per key and remembers the result (a failure serves the
//! original, loudly once). The build decodes the original, applies the ops
//! and re-encodes via a9-formats, budget-checked, then writes the cache
//! atomically (tmp + rename) beside its staleness key.
//!
//! Two keys are special. `csvp` merges the collected table ops; `i.dat` is the
//! M5 W1 master-index merge ([`merge_idat`]) whose order is load-bearing for
//! atomicity: host packs are built FIRST, then rows whose `off` comes from the
//! same `plan_family_a` the host merges used — so no row can point past a
//! vanilla image when a host cache fails.

use super::{Inner, pack_file_name};
use a9_formats::overlay::ImageOp;
use std::path::{Path, PathBuf};

/// merged-container decoded-size ceiling (the engine decode workspace is
/// 200 MB; the mod model's asserted budget is ~190 MB)
const BUDGET: usize = 190 * 1024 * 1024;

fn fnv(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// stat fingerprint of a payload file: (mtime ns, size) — 0,0 if unreadable
fn stat_key(p: &Path) -> (u64, u64) {
    match std::fs::metadata(p) {
        Ok(md) => {
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            (mtime, md.len())
        }
        Err(_) => (0, 0),
    }
}

/// Build (or reuse) the cache for `key`; returns the cache path relative to
/// the game CWD ("mods\cache\<pack>.bin"). Failures are remembered and
/// repeat as loud-once errors.
///
/// The built-set lock is **not** held across the build: a merged's `i.dat`
/// build recurses into its host packs' caches (atomicity — hosts first), so a
/// re-entrant `ensure_cache` must not deadlock. The game's boot loaders are
/// sequential single-threaded, so a check/build/insert without holding the
/// lock across the build is safe; a redundant concurrent build would still
/// write the same content-hashed bytes.
pub(super) fn ensure_cache(inner: &Inner, key: &str) -> Result<PathBuf, String> {
    {
        let built = inner
            .built
            .lock()
            .map_err(|_| "cache state poisoned".to_string())?;
        if let Some(r) = built.get(key) {
            return r.clone();
        }
    }
    let result = build_cache(inner, key);
    match &result {
        Ok((p, true)) => inner
            .logger
            .line(&format!("overlay {key}: cache fresh — reusing {}", p.display())),
        Ok((_, false)) => {} // build_cache logged the merge
        Err(e) => inner
            .logger
            .err(&format!("overlay {key}: {e} — serving the original this boot")),
    }
    let out = result.map(|(p, _)| p);
    let mut built = inner
        .built
        .lock()
        .map_err(|_| "cache state poisoned".to_string())?;
    built.insert(key.to_string(), out.clone());
    out
}

/// The merge: read the original pack, apply the ops, write the cache
/// atomically (tmp + rename) + the key file. Returns (serve path,
/// cache-was-fresh-reused).
///
/// `key == "i.dat"` is the M5 W1 master-index merge (see [`merge_idat`]):
/// host packs first (atomicity), then rows whose `off` comes from the same
/// `plan_family_a` the host merges used.
fn build_cache(inner: &Inner, key: &str) -> Result<(PathBuf, bool), String> {
    let cache_dir = inner.mods_dir.join("cache");
    std::fs::create_dir_all(&cache_dir).map_err(|e| format!("cache dir: {e}"))?;
    let name = pack_file_name(key);
    let original_path = inner.exe_dir.join("data").join(&name);
    let original = std::fs::read(&original_path)
        .map_err(|e| format!("reading the original {}: {e}", original_path.display()))?;

    let cache_path = cache_dir.join(&name);
    let key_path = cache_dir.join(format!("{key}.key"));
    // the path we hand the game's own open: <mods_dir>\cache\<name>,
    // relative to the game CWD
    let serve = inner.mods_dir_rel.join("cache").join(&name);

    // staleness: rebuild only when the key changed (or the cache is missing)
    let expected_key = format!("{:016x}\n", cache_fingerprint(inner, key));
    if cache_path.is_file()
        && std::fs::read_to_string(&key_path).map(|k| k == expected_key).unwrap_or(false)
    {
        return Ok((serve, true)); // fresh cache — reuse
    }

    let bytes = merge_pack(inner, key, &original)?;

    // atomic write: tmp + rename
    let tmp = cache_dir.join(format!("{key}.tmp"));
    std::fs::write(&tmp, &bytes).map_err(|e| format!("writing cache: {e}"))?;
    std::fs::rename(&tmp, &cache_path).map_err(|e| format!("renaming cache: {e}"))?;
    std::fs::write(&key_path, expected_key).map_err(|e| format!("writing cache key: {e}"))?;
    Ok((serve, false))
}

/// The content hash that decides cache staleness: FNV-1a over the pack key and
/// every op's identity (mod id + version + entry name + payload bytes, or the
/// payload file's mtime + size for the read-at-merge families). A changed mod
/// set or a touched payload file changes this; an unchanged set boots from
/// cache with zero merge cost.
fn cache_fingerprint(inner: &Inner, key: &str) -> u64 {
    let mut fingerprint = Vec::new();
    fingerprint.extend_from_slice(key.as_bytes());
    if key == "csvp" {
        for op in &inner.packs.csvs {
            fingerprint.extend_from_slice(op.mod_id.as_bytes());
            fingerprint.extend_from_slice(op.mod_version.as_bytes());
            fingerprint.extend_from_slice(op.name.as_bytes());
            // row/replace kind + the payload bytes (content-addressed — no
            // file stat needed; the engine already read them at boot)
            fingerprint.push(matches!(op.kind, a9_formats::overlay::CsvOpKind::Rows) as u8);
            fingerprint.extend_from_slice(&(op.payload.len() as u64).to_le_bytes());
            fingerprint.extend_from_slice(&op.payload);
            for k in &op.row_keys {
                fingerprint.extend_from_slice(k.as_bytes());
                fingerprint.push(0);
            }
        }
    } else if key == "i.dat" {
        for op in &inner.packs.assets {
            fingerprint.extend_from_slice(op.mod_id.as_bytes());
            fingerprint.extend_from_slice(op.mod_version.as_bytes());
            fingerprint.extend_from_slice(op.host.as_bytes());
            fingerprint.extend_from_slice(op.name.as_bytes());
            fingerprint.extend_from_slice(op.dir.as_bytes());
            fingerprint.push(op.override_vanilla as u8);
            let (m, s) = stat_key(&op.mod_dir.join(&op.file));
            fingerprint.extend_from_slice(&m.to_le_bytes());
            fingerprint.extend_from_slice(&s.to_le_bytes());
        }
        for l in &inner.packs.listings {
            fingerprint.extend_from_slice(b"listing:");
            fingerprint.extend_from_slice(l.name.as_bytes());
            fingerprint.extend_from_slice(&(l.payload.len() as u64).to_le_bytes());
            fingerprint.extend_from_slice(&l.payload);
        }
    } else if let Some(ops) = inner.packs.images.get(key) {
        for op in ops {
            fingerprint.extend_from_slice(op.mod_id.as_bytes());
            fingerprint.extend_from_slice(op.mod_version.as_bytes());
            fingerprint.extend_from_slice(op.name.as_bytes());
            fingerprint.extend_from_slice(&op.width.to_le_bytes());
            fingerprint.extend_from_slice(&op.height.to_le_bytes());
            let (m, s) = stat_key(&op.mod_dir.join(&op.file));
            fingerprint.extend_from_slice(&m.to_le_bytes());
            fingerprint.extend_from_slice(&s.to_le_bytes());
        }
    } else {
        // a family-A [[asset]] host pack (assets, then any auto-merged listing)
        for op in inner.packs.assets.iter().filter(|a| a.host == key) {
            fingerprint.extend_from_slice(op.mod_id.as_bytes());
            fingerprint.extend_from_slice(op.mod_version.as_bytes());
            fingerprint.extend_from_slice(op.name.as_bytes());
            fingerprint.extend_from_slice(op.dir.as_bytes());
            let (m, s) = stat_key(&op.mod_dir.join(&op.file));
            fingerprint.extend_from_slice(&m.to_le_bytes());
            fingerprint.extend_from_slice(&s.to_le_bytes());
        }
        for l in inner.packs.listings.iter().filter(|l| l.host == key) {
            fingerprint.extend_from_slice(b"listing:");
            fingerprint.extend_from_slice(l.name.as_bytes());
            fingerprint.extend_from_slice(&(l.payload.len() as u64).to_le_bytes());
            fingerprint.extend_from_slice(&l.payload);
        }
    }
    fnv(&fingerprint)
}

/// Apply the ops for `key` and return the re-encoded pack bytes. `key` names
/// the family: the csvp table container, the i.dat master index, a family-B
/// image pack, or a family-A `[[asset]]` host.
fn merge_pack(inner: &Inner, key: &str, original: &[u8]) -> Result<Vec<u8>, String> {
    if key == "csvp" {
        let ops: Vec<a9_formats::overlay::CsvOp> = inner
            .packs
            .csvs
            .iter()
            .map(|op| a9_formats::overlay::CsvOp {
                name: op.name.clone(),
                kind: op.kind,
                payload: op.payload.clone(),
            })
            .collect();
        let tables = {
            let mut v: Vec<&str> = inner.packs.csvs.iter().map(|o| o.name.as_str()).collect();
            v.sort_unstable();
            v.dedup();
            v.len()
        };
        let (b, report) = a9_formats::overlay::overlay_csvp(original, &ops, BUDGET)
            .map_err(|e| format!("merge: {e}"))?;
        let rows = if report.row_replaced + report.row_appended > 0 {
            format!(
                ", {} row patch(es) ({} replaced, {} added)",
                report.row_replaced + report.row_appended,
                report.row_replaced,
                report.row_appended
            )
        } else {
            String::new()
        };
        inner.logger.line(&format!(
            "overlay csvp: {} table(s) merged ({} added, {} replaced{rows}; {:.1} MB decoded)",
            tables,
            report.added,
            report.replaced,
            report.decoded as f64 / (1024.0 * 1024.0),
        ));
        Ok(b)
    } else if key == "i.dat" {
        merge_idat(inner, original)
    } else if inner.packs.images.contains_key(key) {
        let ops: Vec<ImageOp> = inner
            .packs
            .images
            .get(key)
            .map(|ops| {
                ops.iter()
                    .map(|op| {
                        let payload = std::fs::read(op.mod_dir.join(&op.file)).map_err(|e| {
                            format!("mod {} entry {}: {e}", op.mod_id, op.file.display())
                        })?;
                        Ok(ImageOp {
                            name: op.name.clone(),
                            width: op.width,
                            height: op.height,
                            payload,
                        })
                    })
                    .collect::<Result<Vec<_>, String>>()
            })
            .unwrap_or(Ok(Vec::new()))?;
        let (b, report) = a9_formats::overlay::overlay_pack_b(original, &ops, BUDGET)
            .map_err(|e| format!("merge: {e}"))?;
        inner.logger.line(&format!(
            "overlay {key}: {} entr{} merged ({} added, {} replaced; {:.1} MB decoded)",
            ops.len(),
            if ops.len() == 1 { "y" } else { "ies" },
            report.added,
            report.replaced,
            report.decoded as f64 / (1024.0 * 1024.0),
        ));
        Ok(b)
    } else {
        merge_family_a_host(inner, key, original)
    }
}

/// The full ordered payload list for one family-A host pack: its `[[asset]]`
/// ops (payloads read from the mod dirs) followed by its auto-merged `.dir`
/// listings. The pack merge and the i.dat rows both take their offsets from
/// this one list, so the two can never disagree.
fn host_ops(inner: &Inner, host: &str) -> Result<Vec<a9_formats::overlay::AssetOp>, String> {
    let mut ops: Vec<a9_formats::overlay::AssetOp> = inner
        .packs
        .assets
        .iter()
        .filter(|a| a.host == host)
        .map(|a| {
            let payload = std::fs::read(a.mod_dir.join(&a.file)).map_err(|e| {
                format!("mod {} asset {}: {e}", a.mod_id, a.file.display())
            })?;
            Ok(a9_formats::overlay::AssetOp {
                name: a.name.clone(),
                dir: a.dir.clone(),
                payload,
            })
        })
        .collect::<Result<_, String>>()?;
    // the listings ALWAYS follow the assets, in listing order — the layout the
    // tailgate planner uses too
    ops.extend(
        inner
            .packs
            .listings
            .iter()
            .filter(|l| l.host == host)
            .map(|l| a9_formats::overlay::AssetOp {
                name: l.name.clone(),
                dir: l.dir.clone(),
                payload: l.payload.clone(),
            }),
    );
    Ok(ops)
}

/// The family-A host-pack merge: append the host's `[[asset]]` payloads and its
/// auto-merged `.dir` listings to the decoded stream
/// (`merged = V ++ P0 .. Pn`), re-encode, budget-checked.
fn merge_family_a_host(inner: &Inner, key: &str, original: &[u8]) -> Result<Vec<u8>, String> {
    let ops = host_ops(inner, key)?;
    let (b, report) = a9_formats::overlay::overlay_pack_a(original, &ops, BUDGET)
        .map_err(|e| format!("merge: {e}"))?;
    let listings = ops.len() - inner.packs.assets.iter().filter(|a| a.host == key).count();
    let listing_note = if listings > 0 {
        format!(" + {listings} listing(s)")
    } else {
        String::new()
    };
    inner.logger.line(&format!(
        "overlay {key}: {} asset{} appended ({} row{}{listing_note}; {:.1} MB decoded)",
        ops.len() - listings,
        if ops.len() - listings == 1 { "" } else { "s" },
        ops.len(),
        if ops.len() == 1 { "" } else { "s" },
        report.decoded as f64 / (1024.0 * 1024.0),
    ));
    Ok(b)
}

/// The M5 W1 master-index merge. Order is load-bearing for atomicity:
/// 1. read + parse the shipped `i.dat`;
/// 2. refuse unresolved name collisions — a name that exists in the shipped
///    index without `override_vanilla` (**the authoritative check**: this is
///    the one place the parsed vanilla table is in hand);
/// 3. build every referenced host pack's cache (any failure refuses this
///    merge too — no row may point past a vanilla image);
/// 4. synthesize rows from the same `plan_family_a` the host merges used;
/// 5. serialize + family-A re-encode.
fn merge_idat(inner: &Inner, original: &[u8]) -> Result<Vec<u8>, String> {
    // 1. the shipped index
    let decoded = a9_formats::pack::decode_pack(original, a9_formats::pack::Family::A)
        .map_err(|e| format!("decoding the shipped i.dat: {e}"))?;
    let mut idat = a9_formats::idat::parse(&decoded)
        .map_err(|e| format!("parsing the shipped i.dat: {e}"))?;

    // 2. vanilla collisions — refused before any host pack is built
    for op in &inner.packs.assets {
        if idat.row(&op.name).is_some() && !op.override_vanilla {
            return Err(format!(
                "asset {:?} (mod {}) collides with a SHIPPED i.dat row — set \
                 `override_vanilla = true` on its [[asset]] section to replace it \
                 (the master map's insert-or-find would otherwise silently \
                 overwrite the shipped asset)",
                op.name, op.mod_id
            ));
        }
    }

    // 3. host packs first, in first-appearance order (dedup); a host the
    //    tailgate serves needs no cache built — its payloads are appended in
    //    place at the decode (and a host failure there refuses its own append
    //    without a cache to dangle)
    let hosts = idat_hosts(inner);
    build_host_caches(inner, &hosts)?;

    // 4. rows — off from the shared planner, the same ordered op list the
    //    host merges used. A tailgated host's base is its vanilla DECLARED
    //    size (the value the runtime stub asserts); a merged host's is its
    //    decoded length (what the re-encoded cache actually carries). The two
    //    are equal for vanilla packs, so the rows agree either way.
    let rows = idat_rows(inner, &hosts)?;

    // 5. apply + serialize + re-encode
    let replaced = rows.iter().filter(|r| idat.row(&r.name).is_some()).count();
    idat.add_rows(&rows).map_err(|e| format!("i.dat rows: {e}"))?;
    let out = a9_formats::idat::serialize(&idat);
    let file = a9_formats::pack::encode_pack_a(&out);
    inner.logger.line(&format!(
        "overlay i.dat: {} row{} merged ({} new, {} replaced; {} host pack{}; {:.1} MB decoded)",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" },
        rows.len() - replaced,
        replaced,
        hosts.len(),
        if hosts.len() == 1 { "" } else { "s" },
        out.len() as f64 / (1024.0 * 1024.0),
    ));
    Ok(file)
}

/// Step 3's host list: every referenced host in first-appearance order
/// (assets then listings), deduped.
fn idat_hosts(inner: &Inner) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    let note_host = |hosts: &mut Vec<String>, h: &str| {
        if !hosts.iter().any(|x| x == h) {
            hosts.push(h.to_string());
        }
    };
    for op in &inner.packs.assets {
        note_host(&mut hosts, &op.host);
    }
    for l in &inner.packs.listings {
        note_host(&mut hosts, &l.host);
    }
    hosts
}

/// Step 3: build every non-tailgated host pack's cache first (a host failure
/// refuses the whole index — atomicity).
fn build_host_caches(inner: &Inner, hosts: &[String]) -> Result<(), String> {
    for host in hosts {
        if inner.packs.tailgated.contains(host) {
            continue;
        }
        ensure_cache(inner, host).map_err(|e| format!("host pack {host}: {e}"))?;
    }
    Ok(())
}

/// Step 4: synthesize the i.dat rows from the same `plan_family_a` the host
/// merges used. A tailgated host's base is its vanilla declared size (the
/// value the runtime stub asserts); a merged host's is its decoded length
/// (what the re-encoded cache actually carries) — equal for vanilla packs.
fn idat_rows(inner: &Inner, hosts: &[String]) -> Result<Vec<a9_formats::idat::NewRow>, String> {
    let mut rows: Vec<a9_formats::idat::NewRow> = Vec::new();
    for host in hosts {
        let base = if inner.packs.tailgated.contains(host) {
            *inner.packs.declared.get(host).ok_or_else(|| {
                format!("tailgated host {host} has no boot-time declared size")
            })?
        } else {
            let host_path = inner.exe_dir.join("data").join(format!("{host}.bin"));
            let host_file = std::fs::read(&host_path)
                .map_err(|e| format!("reading the host {host}: {e}"))?;
            a9_formats::pack::decode_pack(&host_file, a9_formats::pack::Family::A)
                .map_err(|e| format!("decoding the host {host}: {e}"))?
                .len()
        };
        let ops = host_ops(inner, host)?;
        let plan = a9_formats::overlay::plan_family_a(base, &ops);
        for (op, (off, len)) in ops.iter().zip(plan) {
            rows.push(a9_formats::idat::NewRow {
                name: op.name.clone(),
                dir: op.dir.clone(),
                len: len as u32,
                flag: 0,
                off: off as u32,
            });
        }
    }
    Ok(rows)
}
