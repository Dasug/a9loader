//! Dependency resolution — the pure planner. Given the parsed
//! manifests and the loader config's order/disabled/allow-conflicts knobs it
//! produces the execution order, the skip list with reasons, warnings, and
//! the effect-overlap report. The loader executes exactly this; `a9ml list`
//! and `doctor` reproduce it offline.
//!
//! Rules:
//! - order: manifest `priority` (lower = earlier), ties by id; overridden by
//!   the config `[order]` per-mod priorities and/or an explicit `sequence`
//!   (listed ids first, in listed order; unlisted mods keep priority order
//!   behind them).
//! - every mod implicitly **supplies its own id**; `requires: X` is satisfied
//!   by any enabled mod whose id or `supplies` covers X; unsatisfied → the
//!   mod and everything depending on it are skipped (fixpoint).
//! - `conflicts` is symmetric; default: the lower-priority mod of the pair is
//!   skipped; `allow_conflicts = true` downgrades to a warning (both load).
//! - two enabled mods writing the same target key = overlap: the earlier mod
//!   keeps the target, later effects are refused (reported to the loader).
//!
//! [`resolve`] is a thin dispatcher over the named steps below, one per rule.

use crate::manifest::{CsvMode, Manifest};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default, Debug, Clone)]
pub struct ResolveConfig {
    /// explicit total order: listed ids first, then the rest by priority
    pub sequence: Option<Vec<String>>,
    /// per-mod priority overrides (mod id -> priority)
    pub priorities: BTreeMap<String, i64>,
    /// disabled mod ids (from a9loader.toml)
    pub disabled: Vec<String>,
    /// true: conflicts warn + load anyway
    pub allow_conflicts: bool,
}

#[derive(Debug, Clone)]
pub struct Skip {
    pub idx: usize,
    pub id: String,
    pub reason: String,
}

/// Two or more enabled effects writing the same target key. `owners` is in
/// execution order — the first owner keeps the target, the rest are refused.
#[derive(Debug, Clone)]
pub struct Overlap {
    pub key: String,
    pub owners: Vec<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct Resolution {
    /// enabled mod indices, in execution order
    pub order: Vec<usize>,
    pub skipped: Vec<Skip>,
    pub warnings: Vec<String>,
    pub overlaps: Vec<Overlap>,
}

impl Resolution {
    /// true when the given mod's effect on `key` must be refused (it lost an
    /// overlap to an earlier mod)
    pub fn effect_refused(&self, mod_idx: usize, key: &str) -> bool {
        self.overlaps
            .iter()
            .any(|o| o.key == key && o.owners.first() != Some(&mod_idx) && o.owners.contains(&mod_idx))
    }
}

pub fn resolve(mods: &[Manifest], cfg: &ResolveConfig) -> Resolution {
    let mut res = Resolution::default();

    // 1. base order: priority (with overrides), ties by id.
    let mut order = base_order(mods, cfg);

    // 2. explicit sequence pulls listed ids to the front, in listed order.
    apply_sequence(&mut order, mods, cfg);

    // 3. disabled
    let mut enabled = apply_disabled(&mut res, &order, mods, cfg);

    // 4. requires — fixpoint: an unsatisfied requirement disables the mod,
    //    which may unsatisfy its dependents.
    apply_requires_fixpoint(&mut res, &mut enabled, mods);

    // 5. conflicts — symmetric; default skips the lower-priority (later) mod.
    apply_conflicts(&mut res, &mut enabled, mods, cfg);

    // 6. overlaps — same target key written by two enabled mods.
    collect_overlaps(&mut res, &enabled, mods);

    res.order = enabled;
    res
}

/// Step 1 — base order: priority (with overrides), ties by id.
fn base_order(mods: &[Manifest], cfg: &ResolveConfig) -> Vec<usize> {
    let prio = |i: usize| -> i64 {
        let m = &mods[i];
        *cfg.priorities.get(&m.id).unwrap_or(&m.priority)
    };
    let mut order: Vec<usize> = (0..mods.len()).collect();
    order.sort_by(|&a, &b| prio(a).cmp(&prio(b)).then(mods[a].id.cmp(&mods[b].id)));
    order
}

/// Step 2 — an explicit `sequence` pulls listed ids to the front, in listed
/// order; unlisted mods keep their base order behind them.
fn apply_sequence(order: &mut Vec<usize>, mods: &[Manifest], cfg: &ResolveConfig) {
    let Some(seq) = &cfg.sequence else { return };
    let mut front = Vec::new();
    for id in seq {
        if let Some(i) = mods.iter().position(|m| &m.id == id) {
            if !front.contains(&i) {
                front.push(i);
            }
        }
    }
    let rest: Vec<usize> = order.iter().copied().filter(|i| !front.contains(i)).collect();
    *order = front.into_iter().chain(rest).collect();
}

/// Step 3 — drop disabled mods, recording each as a skip.
fn apply_disabled(
    res: &mut Resolution,
    order: &[usize],
    mods: &[Manifest],
    cfg: &ResolveConfig,
) -> Vec<usize> {
    let disabled: BTreeSet<&str> = cfg.disabled.iter().map(|s| s.trim()).collect();
    let mut enabled: Vec<usize> = Vec::new();
    for &i in order {
        if disabled.contains(mods[i].id.as_str()) {
            res.skipped.push(Skip {
                idx: i,
                id: mods[i].id.clone(),
                reason: "disabled in a9loader.toml".into(),
            });
        } else {
            enabled.push(i);
        }
    }
    enabled
}

/// Step 4 — `requires` fixpoint: an unsatisfied requirement disables the mod,
/// which may in turn unsatisfy its dependents.
fn apply_requires_fixpoint(res: &mut Resolution, enabled: &mut Vec<usize>, mods: &[Manifest]) {
    loop {
        let supplies: BTreeSet<&str> = enabled
            .iter()
            .flat_map(|&i| std::iter::once(mods[i].id.as_str()).chain(mods[i].supplies.iter().map(|s| s.as_str())))
            .collect();
        let mut newly_skipped = None;
        'outer: for &i in enabled.iter() {
            for r in &mods[i].requires {
                if !supplies.contains(r.as_str()) {
                    newly_skipped = Some((i, r.clone()));
                    break 'outer;
                }
            }
        }
        match newly_skipped {
            Some((i, missing)) => {
                enabled.retain(|&e| e != i);
                res.skipped.push(Skip {
                    idx: i,
                    id: mods[i].id.clone(),
                    reason: format!("requires {missing:?} (not supplied by any enabled mod)"),
                });
            }
            None => break,
        }
    }
}

/// Step 5 — symmetric conflicts; default skips the lower-priority (later) mod,
/// `allow_conflicts` downgrades to a warning (both load).
fn apply_conflicts(
    res: &mut Resolution,
    enabled: &mut Vec<usize>,
    mods: &[Manifest],
    cfg: &ResolveConfig,
) {
    let mut i = 0;
    while i < enabled.len() {
        let mut j = i + 1;
        while j < enabled.len() {
            let a = enabled[i];
            let b = enabled[j];
            let conflicts = mods[a].conflicts.iter().any(|c| c == &mods[b].id)
                || mods[b].conflicts.iter().any(|c| c == &mods[a].id);
            if conflicts {
                if cfg.allow_conflicts {
                    res.warnings.push(format!(
                        "allow_conflicts: '{}' and '{}' conflict but both load",
                        mods[a].id, mods[b].id
                    ));
                    j += 1;
                } else {
                    let (loser, winner) = (b, a);
                    enabled.retain(|&e| e != loser);
                    res.skipped.push(Skip {
                        idx: loser,
                        id: mods[loser].id.clone(),
                        reason: format!(
                            "conflicts with '{}' (higher priority wins; allow_conflicts to override)",
                            mods[winner].id
                        ),
                    });
                    // do not advance j: the list shifted
                }
            } else {
                j += 1;
            }
        }
        i += 1;
    }
}

/// Step 6 — overlaps: same target key written by two enabled mods
/// (memory effects: anchors/raw targets; data effects: pack entries,
/// csv tables). `train_list` hooks are the one
/// composable effect: several mods may contribute forms and the loader
/// composes them, so a group made ONLY of train_list hooks is
/// not an overlap. A train_list vs a `nop` (or two `nop`s) still is.
fn collect_overlaps(res: &mut Resolution, enabled: &[usize], mods: &[Manifest]) {
    let mut owners: BTreeMap<String, Vec<(usize, bool)>> = BTreeMap::new();
    let mut own = |key: String, i: usize, composable: bool| {
        owners.entry(key).or_default().push((i, composable));
    };
    for &i in enabled {
        for p in &mods[i].patch {
            own(format!("patch:{}", p.target.key(p.offset)), i, false);
        }
        for h in &mods[i].hook {
            let composable = h.handler == "train_list";
            own(format!("hook:{}", h.target.key(0)), i, composable);
        }
        for p in &mods[i].pack {
            for e in &p.entry {
                own(format!("pack:{}:{}", p.pack, e.name), i, false);
            }
        }
        for c in &mods[i].csv {
            // `replace` (whole table) keys as one target; `rows` keys per row
            // so two mods editing different rows of one table coexist while two
            // mods editing the SAME row collide. A `rows` section whose patch
            // file could not be read falls back to the whole-table key
            // (fail-safe: no silent permissiveness).
            if c.mode == CsvMode::Rows && !c.row_keys.is_empty() {
                let mut seen = BTreeSet::new();
                for k in &c.row_keys {
                    let key = format!("csv:{}:{}", c.name, k);
                    if seen.insert(key.clone()) {
                        own(key, i, false);
                    }
                }
            } else {
                own(format!("csv:{}", c.name), i, false);
            }
        }
        for l in &mods[i].loose_asset {
            // the effect key lowercases the pattern: the same pattern from two
            // mods is one redirect, exactly like `asset:<name>` is one row
            own(format!("loose:{}", l.match_.to_ascii_lowercase()), i, false);
        }
        for a in &mods[i].asset {
            // `[[asset]]` names are bare master-map keys — a difference from the
            // family-B pack-entry namespace; colliding with a SHIPPED i.dat row
            // is a separate (load-time) hard refusal.
            own(format!("asset:{}", a.name), i, false);
        }
        // `[[train]]`: a compiler that expands into the row-merge, asset
        // and row-injection effects above — so it registers exactly those keys.
        // Its `hook:train_list_filter` entry is composable (like every
        // train_list contribution); the `csv:`/`asset:`/`train:` keys are not.
        for key in crate::train::effect_keys_of(&mods[i].id, &mods[i].train) {
            let composable = key == "hook:train_list_filter";
            own(key, i, composable);
        }
    }
    for (key, os) in owners {
        if os.len() > 1 && !os.iter().all(|(_, composable)| *composable) {
            res.overlaps.push(Overlap {
                key,
                owners: os.into_iter().map(|(i, _)| i).collect(),
            });
        }
    }
}

#[cfg(test)]
mod tests;
