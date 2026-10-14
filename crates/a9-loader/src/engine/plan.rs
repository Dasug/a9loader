//! The boot's data-plan collector — the manifest → serving-plan translation.
//!
//! [`PlanBuilder`] walks the resolved mods in priority order and folds every
//! `[[pack]]`/`[[csv]]`/`[[asset]]`/`[[file]]`/`[[loose_asset]]` section into
//! the overlay plan (or, for family-B `[[pack]]` effects in inject mode, the
//! injector plan), the `[[file]]` copy list, and the refusal count. It is pure
//! boot-time planning: it reads payload files and logs; the serving installs
//! happen later in the engine.

use crate::log;
use a9_mod::{manifest::Manifest, resolve};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The `[[asset]]` host resolution: `host = "auto"` (the folder-form default)
/// is resolved against the embedded per-build dir index — the boot knows the
/// build. A non-auto host is returned as authored. `Err` = the auto resolution
/// refused (logged); the caller counts the refusal.
type AutoIndex = Option<Result<a9_mod::dirindex::DirIndex, a9_mod::ModError>>;

/// The `[[pack]]`/`[[csv]]`/`[[asset]]`/`[[file]]`/`[[loose_asset]]` plan
/// collector: walks the resolved mods in priority order and fills the overlay
/// plan (or, for family-B `[[pack]]` effects in inject mode, the injector
/// plan), the `[[file]]` copy list, and the refusal count.
pub(super) struct PlanBuilder<'a> {
    res: &'a resolve::Resolution,
    build_id: &'a str,
    inject_mode: bool,
    logger: &'a log::Logger,
    plan: crate::overlay::Plan,
    inject_plan: crate::injector::InjectorPlan,
    file_copies: Vec<(String, a9_mod::manifest::FileSection, PathBuf)>,
    refused: usize,
}

impl<'a> PlanBuilder<'a> {
    pub(super) fn new(
        res: &'a resolve::Resolution,
        build_id: &'a str,
        inject_mode: bool,
        logger: &'a log::Logger,
    ) -> Self {
        Self {
            res,
            build_id,
            inject_mode,
            logger,
            plan: crate::overlay::Plan {
                images: BTreeMap::new(),
                csvs: Vec::new(),
                assets: Vec::new(),
                listings: Vec::new(),
                tailgated: BTreeSet::new(),
                declared: BTreeMap::new(),
                loose: Vec::new(),
            },
            inject_plan: crate::injector::InjectorPlan::default(),
            file_copies: Vec::new(),
            refused: 0,
        }
    }

    /// Collect one mod's data effects, in the documented section order:
    /// `[[pack]]`, `[[csv]]`, `[[asset]]`, `[[file]]`, `[[loose_asset]]`.
    pub(super) fn add_mod(&mut self, i: usize, m: &Manifest, dir: &Path) {
        self.packs(i, m, dir);
        self.csvs(i, m, dir);
        self.assets(i, m, dir);
        self.files(i, m, dir);
        self.loose(i, m, dir);
    }

    pub(super) fn finish(
        self,
    ) -> (
        crate::overlay::Plan,
        crate::injector::InjectorPlan,
        Vec<(String, a9_mod::manifest::FileSection, PathBuf)>,
        usize,
    ) {
        (self.plan, self.inject_plan, self.file_copies, self.refused)
    }

    /// `[[pack]]` — family-B image packs only. In inject mode the ops go to the
    /// injector plan; otherwise to the merge overlay.
    fn packs(&mut self, i: usize, m: &Manifest, dir: &Path) {
        for p in &m.pack {
            // only family-B image packs carry entries (me*/mt*); family-A
            // packs (tp/mcp/ep/cp/vp) are size-prefixed streams with no
            // entry table, and csvp tables belong in [[csv]]
            if !matches!(
                a9_formats::pack::classify(&p.pack),
                Some((a9_formats::pack::Family::B, a9_formats::pack::Container::Images))
            ) {
                self.logger.err(&format!(
                    "mod '{}': pack {} is not a family-B image pack — [[pack]] entries \
                     unsupported there (family-A stream or [[csv]] territory); skipped",
                    m.id, p.pack
                ));
                self.refused += 1;
                continue;
            }
            for e in &p.entry {
                let key = format!("pack:{}:{}", p.pack, e.name);
                if self.res.effect_refused(i, &key) {
                    self.logger
                        .err(&format!("mod '{}': {key} refused (overlap loser)", m.id));
                    self.refused += 1;
                    continue;
                }
                let target = if self.inject_mode {
                    self.inject_plan.packs.entry(p.pack.clone()).or_default()
                } else {
                    self.plan.images.entry(p.pack.clone()).or_default()
                };
                target.push((dir.to_path_buf(), m.id.clone(), m.version.to_string(), e.clone()));
            }
        }
    }

    /// `[[csv]]` — the csvp table merge (both serving modes). A `mode = "rows"`
    /// patch is filtered per row against the resolver's overlap winners.
    fn csvs(&mut self, i: usize, m: &Manifest, dir: &Path) {
        for c in &m.csv {
            let payload = match std::fs::read(dir.join(&c.file)) {
                Ok(b) => b,
                Err(e) => {
                    self.logger.err(&format!(
                        "mod '{}': csv {} payload {}: {e} — effect skipped",
                        m.id,
                        c.name,
                        c.file
                    ));
                    self.refused += 1;
                    continue;
                }
            };
            match c.mode {
                a9_mod::manifest::CsvMode::Replace => self.csv_replace(i, m, c, payload),
                a9_mod::manifest::CsvMode::Rows => self.csv_rows(i, m, c, payload),
            }
        }
    }

    /// `mode = "replace"`: the whole table is replaced by the payload.
    fn csv_replace(
        &mut self,
        i: usize,
        m: &Manifest,
        c: &a9_mod::manifest::CsvSection,
        payload: Vec<u8>,
    ) {
        let key = format!("csv:{}", c.name);
        if self.res.effect_refused(i, &key) {
            self.logger
                .err(&format!("mod '{}': {key} refused (overlap loser)", m.id));
            self.refused += 1;
            return;
        }
        self.plan.csvs.push(crate::overlay::CsvOpPlan {
            mod_id: m.id.clone(),
            mod_version: m.version.to_string(),
            kind: a9_formats::overlay::CsvOpKind::Replace,
            name: c.name.clone(),
            payload,
            row_keys: Vec::new(),
        });
    }

    /// `mode = "rows"`: a row patch; rows another mod already owns are dropped
    /// here (per-row overlap refusal), the rest apply.
    fn csv_rows(
        &mut self,
        i: usize,
        m: &Manifest,
        c: &a9_mod::manifest::CsvSection,
        payload: Vec<u8>,
    ) {
        let res = self.res;
        // whole-table fallback key (the patch file was unreadable at
        // scan time -> row_keys empty): fail-safe refusal if spent
        let table_key = format!("csv:{}", c.name);
        if res.effect_refused(i, &table_key) {
            self.logger
                .err(&format!("mod '{}': {table_key} refused (overlap loser)", m.id));
            self.refused += 1;
            return;
        }
        let mut kept = match a9_formats::csv::parse_patch(&payload) {
            Ok(t) => t,
            Err(e) => {
                self.logger.err(&format!(
                    "mod '{}': csv {} row patch {}: {e} — effect skipped",
                    m.id, c.name, c.file
                ));
                self.refused += 1;
                return;
            }
        };
        let mut dropped = 0usize;
        let mut keys: Vec<String> = Vec::new();
        kept.retain_rows(|r| {
            let k = r.key();
            if k.is_empty() {
                return true; // the merge refuses this loudly
            }
            if res.effect_refused(i, &format!("csv:{}:{}", c.name, k)) {
                dropped += 1;
                false
            } else {
                keys.push(k.to_string());
                true
            }
        });
        if dropped > 0 {
            self.logger.err(&format!(
                "mod '{}': csv {} — {dropped} row patch(es) refused (overlap loser)",
                m.id, c.name
            ));
            self.refused += dropped;
        }
        if kept.is_empty() {
            return;
        }
        self.plan.csvs.push(crate::overlay::CsvOpPlan {
            mod_id: m.id.clone(),
            mod_version: m.version.to_string(),
            kind: a9_formats::overlay::CsvOpKind::Rows,
            name: c.name.clone(),
            payload: kept.serialize(),
            row_keys: keys,
        });
    }

    /// `[[asset]]` — new i.dat rows with payloads appended to a family-A host
    /// pack (M5 W1). Always merge-served: inject has no family-A seam.
    fn assets(&mut self, i: usize, m: &Manifest, dir: &Path) {
        // `host = "auto"` (the folder-form default) is resolved here, against
        // the embedded per-build dir index — the boot knows the build.
        let auto_index: AutoIndex = m
            .asset
            .iter()
            .any(|a| a.host.eq_ignore_ascii_case(a9_mod::manifest::HOST_AUTO))
            .then(a9_mod::dirindex::embedded_index);
        for a in &m.asset {
            let host = match self.resolve_asset_host(m, a, &auto_index) {
                Ok(h) => h,
                Err(()) => {
                    self.refused += 1;
                    continue;
                }
            };
            if !matches!(
                a9_formats::pack::classify(&host),
                Some((a9_formats::pack::Family::A, a9_formats::pack::Container::Opaque))
            ) {
                self.logger.err(&format!(
                    "mod '{}': asset {} host {} is not a family-A pack the loader \
                     can append to (vp/tp/mcp/ep/cp); skipped",
                    m.id, a.name, host
                ));
                self.refused += 1;
                continue;
            }
            let key = format!("asset:{}", a.name);
            if self.res.effect_refused(i, &key) {
                self.logger.err(&format!(
                    "mod '{}': {key} refused (overlap loser — an earlier mod already \
                     provides this asset name)",
                    m.id
                ));
                self.refused += 1;
                continue;
            }
            self.plan.assets.push(crate::overlay::AssetOpPlan {
                mod_id: m.id.clone(),
                mod_version: m.version.to_string(),
                host,
                name: a.name.clone(),
                dir: a.dir.clone(),
                mod_dir: dir.to_path_buf(),
                file: a.file.clone(),
                override_vanilla: a.override_vanilla,
            });
        }
    }

    /// Resolve one `[[asset]]`'s host: an authored host is returned as-is; an
    /// `auto` host goes through the embedded dir index (a refusal is logged).
    fn resolve_asset_host(
        &self,
        m: &Manifest,
        a: &a9_mod::manifest::AssetSection,
        auto_index: &AutoIndex,
    ) -> Result<String, ()> {
        if !a.host.eq_ignore_ascii_case(a9_mod::manifest::HOST_AUTO) {
            return Ok(a.host.clone());
        }
        match auto_index {
            Some(Ok(idx)) => {
                match a9_mod::dirindex::resolve_auto(idx, self.build_id, &a.name, &a.dir) {
                    Ok(h) => {
                        self.logger.line(&format!(
                            "mod '{}': asset {}: host auto -> {h} (dir index)",
                            m.id, a.name
                        ));
                        Ok(h)
                    }
                    Err(e) => {
                        self.logger.err(&format!(
                            "mod '{}': asset {}: host auto refused for dir {:?}: {e} \
                             — effect skipped",
                            m.id, a.name, a.dir
                        ));
                        Err(())
                    }
                }
            }
            Some(Err(e)) => {
                self.logger.err(&format!(
                    "mod '{}': asset {}: host auto refused — the embedded dir index \
                     failed to parse ({e})",
                    m.id, a.name
                ));
                Err(())
            }
            None => {
                self.logger.err(&format!(
                    "mod '{}': asset {}: host auto refused — no dir index for build {}",
                    m.id, a.name, self.build_id
                ));
                Err(())
            }
        }
    }

    /// `[[file]]` — genuinely-new files, copied before the game enumerates
    /// anything (idempotent, never overwrites).
    fn files(&mut self, i: usize, m: &Manifest, dir: &Path) {
        for f in &m.file {
            let key = format!("file:{}", f.dest);
            if self.res.effect_refused(i, &key) {
                self.logger
                    .err(&format!("mod '{}': {key} refused (overlap loser)", m.id));
                self.refused += 1;
                continue;
            }
            self.file_copies.push((m.id.clone(), f.clone(), dir.to_path_buf()));
        }
    }

    /// `[[loose_asset]]` — game files opened by path, served from a
    /// loader-managed cache copy (M6). The payload is materialized at overlay
    /// install time; a failure there drops the asset loudly.
    fn loose(&mut self, i: usize, m: &Manifest, dir: &Path) {
        for l in &m.loose_asset {
            let key = format!("loose:{}", l.match_.to_ascii_lowercase());
            if self.res.effect_refused(i, &key) {
                self.logger.err(&format!(
                    "mod '{}': {key} refused (overlap loser — an earlier mod already \
                     serves this path pattern)",
                    m.id
                ));
                self.refused += 1;
                continue;
            }
            self.plan.loose.push(crate::overlay::LooseOpPlan {
                mod_id: m.id.clone(),
                pattern: l.match_.clone(),
                mod_dir: dir.to_path_buf(),
                file: l.file.clone(),
            });
        }
    }
}
