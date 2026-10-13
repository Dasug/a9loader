//! Dependency resolution — the pure planner (PLAN.md §4.8). Given the parsed
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
    let prio = |i: usize| -> i64 {
        let m = &mods[i];
        *cfg.priorities.get(&m.id).unwrap_or(&m.priority)
    };
    let mut order: Vec<usize> = (0..mods.len()).collect();
    order.sort_by(|&a, &b| prio(a).cmp(&prio(b)).then(mods[a].id.cmp(&mods[b].id)));

    // 2. explicit sequence pulls listed ids to the front, in listed order.
    if let Some(seq) = &cfg.sequence {
        let mut front = Vec::new();
        for id in seq {
            if let Some(i) = mods.iter().position(|m| &m.id == id) {
                if !front.contains(&i) {
                    front.push(i);
                }
            }
        }
        let rest: Vec<usize> = order.iter().copied().filter(|i| !front.contains(i)).collect();
        order = front.into_iter().chain(rest).collect();
    }

    // 3. disabled
    let disabled: BTreeSet<&str> = cfg.disabled.iter().map(|s| s.trim()).collect();
    let mut enabled: Vec<usize> = Vec::new();
    for &i in &order {
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

    // 4. requires — fixpoint: an unsatisfied requirement disables the mod,
    //    which may unsatisfy its dependents.
    loop {
        let supplies: BTreeSet<&str> = enabled
            .iter()
            .flat_map(|&i| std::iter::once(mods[i].id.as_str()).chain(mods[i].supplies.iter().map(|s| s.as_str())))
            .collect();
        let mut newly_skipped = None;
        'outer: for &i in &enabled {
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

    // 5. conflicts — symmetric; default skips the lower-priority (later) mod.
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

    // 6. overlaps — same target key written by two enabled mods
    //    (memory effects: anchors/raw targets; data effects: pack entries,
    //    csv tables, file destinations). `train_list` hooks are the one
    //    composable effect: several mods may contribute forms and the loader
    //    composes them (M5 W3), so a group made ONLY of train_list hooks is
    //    not an overlap. A train_list vs a `nop` (or two `nop`s) still is.
    let mut owners: BTreeMap<String, Vec<(usize, bool)>> = BTreeMap::new();
    let mut own = |key: String, i: usize, composable: bool| {
        owners.entry(key).or_default().push((i, composable));
    };
    for &i in &enabled {
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
        for f in &mods[i].file {
            own(format!("file:{}", f.dest), i, false);
        }
        for a in &mods[i].asset {
            // `[[asset]]` names are bare master-map keys — a difference from the
            // family-B pack-entry namespace; colliding with a SHIPPED i.dat row
            // is a separate (load-time) hard refusal.
            own(format!("asset:{}", a.name), i, false);
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

    res.order = enabled;
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(id: &str, priority: i64, extra: &str) -> Manifest {
        toml::from_str(&format!(
            "id = \"{id}\"\nname = \"{id}\"\nversion = \"1.0.0\"\nauthor = \"t\"\npriority = {priority}\n{extra}"
        ))
        .unwrap()
    }

    fn m(id: &str, priority: i64) -> Manifest {
        build(id, priority, "")
    }

    fn ids(res: &Resolution, mods: &[Manifest]) -> Vec<String> {
        res.order.iter().map(|&i| mods[i].id.clone()).collect()
    }

    fn hook_on(id: &str, anchor: &str) -> Manifest {
        build(
            id,
            50,
            &format!("[[hook]]\ntarget = \"{anchor}\"\nhandler = \"nop\"\n"),
        )
    }

    #[test]
    fn priority_order_with_ties_by_id() {
        let mods = vec![m("b", 10), m("a", 10), m("z", 5)];
        let res = resolve(&mods, &ResolveConfig::default());
        assert_eq!(ids(&res, &mods), vec!["z", "a", "b"]);
    }

    #[test]
    fn overrides_and_sequence() {
        let mods = vec![m("a", 50), m("b", 50), m("c", 50)];
        let mut cfg = ResolveConfig::default();
        cfg.priorities.insert("a".into(), 90);
        let res = resolve(&mods, &cfg);
        assert_eq!(ids(&res, &mods), vec!["b", "c", "a"]);
        cfg.sequence = Some(vec!["c".into(), "a".into()]);
        let res = resolve(&mods, &cfg);
        assert_eq!(ids(&res, &mods), vec!["c", "a", "b"]);
    }

    #[test]
    fn disabled_skips() {
        let mods = vec![m("a", 10), m("b", 20)];
        let mut cfg = ResolveConfig::default();
        cfg.disabled.push("a".into());
        let res = resolve(&mods, &cfg);
        assert_eq!(ids(&res, &mods), vec!["b"]);
        assert!(res.skipped[0].reason.contains("disabled"));
    }

    #[test]
    fn requires_fixpoint_cascades() {
        fn trio() -> Vec<Manifest> {
            // base supplies "cap"; mid requires cap; top requires mid
            let base = build("base", 10, "supplies = [\"cap\"]\n");
            let mid = build("mid", 20, "requires = [\"cap\"]\n");
            let top = build("top", 30, "requires = [\"mid\"]\n");
            vec![top, mid, base]
        }
        fn base_mod() -> Manifest {
            build("base", 10, "supplies = [\"cap\"]\n")
        }
        let mods = trio();
        let res = resolve(&mods, &ResolveConfig::default());
        assert_eq!(ids(&res, &mods), vec!["base", "mid", "top"]);

        // base disabled -> mid and top cascade out
        let mut cfg = ResolveConfig::default();
        cfg.disabled.push("base".into());
        let res = resolve(&mods, &cfg);
        assert!(res.order.is_empty());
        assert_eq!(res.skipped.len(), 3);

        // implicit own-id supply: top requires "base" by id
        let top2 = build("top2", 30, "requires = [\"base\"]\n");
        let mods2 = vec![base_mod(), top2];
        let res = resolve(&mods2, &ResolveConfig::default());
        assert_eq!(ids(&res, &mods2), vec!["base", "top2"]);

        // missing requirement names it
        let lonely = build("lonely", 40, "requires = [\"nothing\"]\n");
        let mods3 = vec![base_mod(), lonely];
        let res = resolve(&mods3, &ResolveConfig::default());
        assert_eq!(ids(&res, &mods3), vec!["base"]);
        assert!(res.skipped[0].reason.contains("\"nothing\""));
    }

    #[test]
    fn conflicts_symmetric_default_skips_lower_priority() {
        let a = build("a", 10, "conflicts = [\"b\"]\n");
        let b = m("b", 20);
        let mods = vec![b, a];
        let res = resolve(&mods, &ResolveConfig::default());
        assert_eq!(ids(&res, &mods), vec!["a"]);
        assert!(res.skipped[0].reason.contains("conflicts with 'a'"));
        // declared on the other side only — still symmetric
        let a2 = m("a2", 10);
        let b2 = build("b2", 20, "conflicts = [\"a2\"]\n");
        let mods = vec![a2, b2];
        let res = resolve(&mods, &ResolveConfig::default());
        assert_eq!(ids(&res, &mods), vec!["a2"]);
        // allow_conflicts: both load with a warning
        let mut cfg = ResolveConfig::default();
        cfg.allow_conflicts = true;
        let res = resolve(&mods, &cfg);
        assert_eq!(ids(&res, &mods), vec!["a2", "b2"]);
        assert!(res.warnings[0].contains("both load"));
    }

    #[test]
    fn overlap_first_owner_wins() {
        let mods = vec![hook_on("first", "train_list_filter"), hook_on("second", "train_list_filter")];
        let res = resolve(&mods, &ResolveConfig::default());
        assert_eq!(ids(&res, &mods), vec!["first", "second"]);
        assert_eq!(res.overlaps.len(), 1);
        // keys are recorded with their effect-type prefix
        assert_eq!(res.overlaps[0].key, "hook:train_list_filter");
        assert!(!res.effect_refused(0, "hook:train_list_filter"));
        assert!(res.effect_refused(1, "hook:train_list_filter"));
        // different targets: no overlap
        let mods = vec![hook_on("a", "train_list_filter"), hook_on("b", "train_list_whitelist")];
        let res = resolve(&mods, &ResolveConfig::default());
        assert!(res.overlaps.is_empty());
    }

    /// A train_list hook (`mode` selects allow/add/deny).
    fn tl_on(id: &str, mode: &str, list: &str) -> Manifest {
        build(
            id,
            50,
            &format!(
                "[[hook]]\ntarget = \"train_list_filter\"\nhandler = \"train_list\"\nmode = \"{mode}\"\nlist = \"{list}\"\n"
            ),
        )
    }

    #[test]
    fn train_list_hooks_compose_without_overlap() {
        // two train_list contributions are composable (M5 W3): no overlap, so
        // the engine folds both into one filter install
        let a = tl_on("a", "allow", "a.txt");
        let b = tl_on("b", "add", "b.txt");
        let res = resolve(&[a, b], &ResolveConfig::default());
        assert!(res.overlaps.is_empty(), "{:?}", res.overlaps);
        // a train_list vs a nop still conflicts on the seam (one writer)
        let c = hook_on("c", "train_list_filter");
        let res = resolve(&[tl_on("d", "add", "d.txt"), c], &ResolveConfig::default());
        assert_eq!(res.overlaps.len(), 1);
        assert_eq!(res.overlaps[0].key, "hook:train_list_filter");
    }

    #[test]
    fn data_effect_overlaps_refuse_later_mods() {
        // two mods adding the same pack entry
        let a = build(
            "aa",
            10,
            "[[pack]]\npack = \"mtentr0\"\n[[pack.entry]]\nname = \"_d_x.dds\"\nwidth = 1\nheight = 1\nfile = \"x.dds\"\n",
        );
        let b = build(
            "bb",
            20,
            "[[pack]]\npack = \"mtentr0\"\n[[pack.entry]]\nname = \"_d_x.dds\"\nwidth = 2\nheight = 2\nfile = \"y.dds\"\n",
        );
        let mods = vec![b, a.clone()];
        let res = resolve(&mods, &ResolveConfig::default());
        assert_eq!(res.overlaps.len(), 1);
        assert_eq!(res.overlaps[0].key, "pack:mtentr0:_d_x.dds");
        assert!(!res.effect_refused(1, "pack:mtentr0:_d_x.dds"));
        assert!(res.effect_refused(0, "pack:mtentr0:_d_x.dds"));
        // different entry names in the same pack: fine
        let b2 = build(
            "bb",
            20,
            "[[pack]]\npack = \"mtentr0\"\n[[pack.entry]]\nname = \"_d_z.dds\"\nwidth = 2\nheight = 2\nfile = \"z.dds\"\n",
        );
        let res = resolve(&vec![b2, a], &ResolveConfig::default());
        assert!(res.overlaps.is_empty());
        // csv + file dests overlap too
        let c1 = build("c1", 10, "[[csv]]\nname = \"t.csv\"\nfile = \"t.csv\"\n");
        let c2 = build("c2", 20, "[[csv]]\nname = \"t.csv\"\nfile = \"u.csv\"\n");
        let res = resolve(&vec![c2, c1], &ResolveConfig::default());
        assert_eq!(res.overlaps[0].key, "csv:t.csv");
        let f1 = build("f1", 10, "[[file]]\ndest = \"data/x.dds\"\nfile = \"x.dds\"\n");
        let f2 = build("f2", 20, "[[file]]\ndest = \"data/x.dds\"\nfile = \"y.dds\"\n");
        let res = resolve(&vec![f2, f1], &ResolveConfig::default());
        assert_eq!(res.overlaps[0].key, "file:data/x.dds");
        // two mods adding the same NEW asset name: overlap, earlier wins,
        // no hard error (the loser's op is skipped by the engine)
        let a1 = build("a1", 10, "[[asset]]\nname=\"_t_dup_a.x\"\ndir=\"train\\\\dup\"\nhost=\"vp\"\nfile=\"x.x\"\n");
        let a2 = build("a2", 20, "[[asset]]\nname=\"_t_dup_a.x\"\ndir=\"train\\\\dup\"\nhost=\"vp\"\nfile=\"y.x\"\n");
        let res = resolve(&vec![a2, a1.clone()], &ResolveConfig::default());
        assert_eq!(res.overlaps.len(), 1);
        assert_eq!(res.overlaps[0].key, "asset:_t_dup_a.x");
        let w = res.order[0];
        let l = res.order[1];
        assert!(!res.effect_refused(w, "asset:_t_dup_a.x"));
        assert!(res.effect_refused(l, "asset:_t_dup_a.x"));
        // different asset names: no overlap
        let a3 = build("a3", 20, "[[asset]]\nname=\"_t_other_a.x\"\ndir=\"train\\\\o\"\nhost=\"vp\"\nfile=\"z.x\"\n");
        assert!(resolve(&vec![a3, a1], &ResolveConfig::default()).overlaps.is_empty());
    }

    #[test]
    fn csv_row_patch_overlaps_are_per_row() {
        fn row_mod(id: &str, prio: i64, keys: &[&str]) -> Manifest {
            let mut m = build(id, prio, "[[csv]]\nname = \"t.csv\"\nfile = \"p.csv\"\nmode = \"rows\"\n");
            m.csv[0].row_keys = keys.iter().map(|s| s.to_string()).collect();
            m
        }
        // same row key -> overlap, earlier mod keeps it
        let a = row_mod("a", 10, &["R1", "R2"]);
        let b = row_mod("b", 20, &["R1"]);
        let mods = vec![b, a];
        let res = resolve(&mods, &ResolveConfig::default());
        assert_eq!(res.overlaps.len(), 1);
        assert_eq!(res.overlaps[0].key, "csv:t.csv:R1");
        let a_idx = res.order[0];
        let b_idx = res.order[1];
        assert!(!res.effect_refused(a_idx, "csv:t.csv:R1"));
        assert!(res.effect_refused(b_idx, "csv:t.csv:R1"));
        // a different row of the same table is fine
        assert!(!res.effect_refused(b_idx, "csv:t.csv:R2"));
        // different rows only -> no overlap at all
        let c = row_mod("c", 10, &["R1"]);
        let d = row_mod("d", 20, &["R2"]);
        assert!(resolve(&vec![d, c], &ResolveConfig::default()).overlaps.is_empty());
        // duplicate keys within one section don't self-overlap
        let dup = row_mod("dup", 10, &["R1", "R1"]);
        assert!(resolve(&vec![dup], &ResolveConfig::default()).overlaps.is_empty());
        // a whole-table replace and a row patch of the same table do not collide
        let rep = build("rep", 10, "[[csv]]\nname = \"t.csv\"\nfile = \"whole.csv\"\n");
        assert!(resolve(&vec![rep, row_mod("rp", 20, &["R1"])], &ResolveConfig::default())
            .overlaps
            .is_empty());
        // unprimed row sections fall back to the whole-table key (fail-safe)
        let u1 = build("u1", 10, "[[csv]]\nname = \"t.csv\"\nfile = \"p.csv\"\nmode = \"rows\"\n");
        let u2 = build("u2", 20, "[[csv]]\nname = \"t.csv\"\nfile = \"q.csv\"\nmode = \"rows\"\n");
        let res = resolve(&vec![u2, u1], &ResolveConfig::default());
        assert_eq!(res.overlaps[0].key, "csv:t.csv");
    }
}
