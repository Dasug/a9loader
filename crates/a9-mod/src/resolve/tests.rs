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
    fn loose_asset_overlap_is_per_pattern() {
        // the same pattern (case-insensitively) from two mods = one redirect:
        // the earlier mod keeps it, the loser's op is skipped by the engine
        let a = build("a", 10, "[[loose_asset]]\nmatch=\"*.adk\\\\fe01.bin\"\nfile=\"x.bin\"\n");
        let b = build("b", 20, "[[loose_asset]]\nmatch=\"*.ADK\\\\FE01.BIN\"\nfile=\"y.bin\"\n");
        let res = resolve(&vec![b, a.clone()], &ResolveConfig::default());
        assert_eq!(res.overlaps.len(), 1);
        assert_eq!(res.overlaps[0].key, "loose:*.adk\\fe01.bin");
        assert!(!res.effect_refused(res.order[0], "loose:*.adk\\fe01.bin"));
        assert!(res.effect_refused(res.order[1], "loose:*.adk\\fe01.bin"));
        // a different pattern does not collide
        let c = build("c", 20, "[[loose_asset]]\nmatch=\"*.adk\\\\fs02.bin\"\nfile=\"z.bin\"\n");
        assert!(resolve(&vec![c, a], &ResolveConfig::default()).overlaps.is_empty());
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
