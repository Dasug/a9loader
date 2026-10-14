//! The csvp table merge (the 0x204 CSV container) — whole-table replaces and
//! per-row patches.

use super::merge_report;
use super::MergeReport;
use crate::container::{self, CsvEntry, NameField};
use crate::csv;
use crate::pack::{self, Family};
use crate::Error;
use std::collections::BTreeMap;

/// One [[csv]] table operation (0x204 containers: csvp).
#[derive(Clone, Debug, PartialEq)]
pub struct CsvOp {
    pub name: String,
    pub kind: CsvOpKind,
    /// [`CsvOpKind::Replace`]: the whole table payload.
    /// [`CsvOpKind::Rows`]: the patch rows (a UTF-16 CSV keyed by first column).
    pub payload: Vec<u8>,
}

/// Whether a [`CsvOp`] replaces the whole table or row-patches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CsvOpKind {
    /// the payload IS the table (M3 whole-table replace)
    Replace,
    /// the payload is a set of row patches merged into the table
    Rows,
    /// the payload is a set of rows appended POSITIONALLY at the end of the
    /// table (M7-2: a `train_pattern` group — only the head row is keyed, so a
    /// keyed [`CsvOpKind::Rows`] patch cannot address the continuation rows).
    /// The rows are placed verbatim; no first-column matching happens.
    GroupAppend,
}

/// Merge csv-table ops into the csvp FILE (family-B, 0x204 container).
pub fn overlay_csvp(
    original_file: &[u8],
    ops: &[CsvOp],
    budget: usize,
) -> Result<(Vec<u8>, MergeReport), Error> {
    let decoded = pack::decode_pack(original_file, Family::B)?;
    overlay_csvs_decoded(&decoded, ops, budget)
}

/// Group op indices by table name, preserving first-appearance order.
fn group_ops_by_table(ops: &[CsvOp]) -> (Vec<String>, BTreeMap<String, Vec<usize>>) {
    let mut order: Vec<String> = Vec::new();
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, op) in ops.iter().enumerate() {
        if !groups.contains_key(&op.name) {
            order.push(op.name.clone());
        }
        groups.entry(op.name.clone()).or_default().push(i);
    }
    (order, groups)
}

/// The base payload for one table: a whole-table [`CsvOpKind::Replace`] op if
/// present (at most one), else the container's existing table; then every
/// `Rows` op for the table merged on top in op order, then every
/// [`CsvOpKind::GroupAppend`] op appended positionally in op order. Returns the
/// bytes plus the `(replaced, appended, group_appended)` row counts the report
/// carries.
fn table_payload(
    name: &str,
    idxs: &[usize],
    ops: &[CsvOp],
    entries: &[CsvEntry],
) -> Result<(Vec<u8>, usize, usize, usize), Error> {
    let replaces: Vec<usize> = idxs
        .iter()
        .copied()
        .filter(|&i| ops[i].kind == CsvOpKind::Replace)
        .collect();
    if replaces.len() > 1 {
        return Err(Error::Container(format!(
            "csv table {name:?}: {} whole-table replaces in one merge",
            replaces.len()
        )));
    }
    let existing = entries.iter().position(|e| e.name.as_str() == *name);
    let mut base_bytes = match (existing, replaces.first()) {
        (_, Some(&ri)) => ops[ri].payload.clone(),
        (Some(p), None) => entries[p].payload.clone(),
        (None, None) => {
            return Err(Error::Container(format!(
                "csv table {name:?}: row patch(es) but the table is not in the container"
            )))
        }
    };
    let row_ops: Vec<&CsvOp> = idxs
        .iter()
        .map(|&i| &ops[i])
        .filter(|o| o.kind == CsvOpKind::Rows)
        .collect();
    let group_ops: Vec<&CsvOp> = idxs
        .iter()
        .map(|&i| &ops[i])
        .filter(|o| o.kind == CsvOpKind::GroupAppend)
        .collect();
    if row_ops.is_empty() && group_ops.is_empty() {
        return Ok((base_bytes, 0, 0, 0));
    }
    let mut table = csv::Table::parse(&base_bytes)?;
    let mut replaced = 0usize;
    let mut appended = 0usize;
    let mut group_appended = 0usize;
    if !row_ops.is_empty() {
        let patches: Vec<csv::Table> = row_ops
            .iter()
            .map(|o| csv::Table::parse(&o.payload))
            .collect::<Result<_, _>>()?;
        let (merged, stats) = csv::merge_rows(&table, &patches, csv::MergeMode::Rows)?;
        table = merged;
        replaced = stats.replaced;
        appended = stats.appended;
    }
    for op in group_ops {
        let group = csv::Table::parse(&op.payload)?;
        let (merged, n) = csv::append_group(&table, &group)?;
        table = merged;
        group_appended += n;
    }
    base_bytes = table.serialize();
    Ok((base_bytes, replaced, appended, group_appended))
}

/// The csv merge on already-decoded container bytes.
///
/// Ops are grouped by table name (first-appearance order). A table with a
/// whole-table [`CsvOpKind::Replace`] op takes that payload as its base; any
/// [`CsvOpKind::Rows`] ops for the same table then apply on top in op order.
/// A row patch for a table the container does not hold is refused loudly.
pub fn overlay_csvs_decoded(
    decoded: &[u8],
    ops: &[CsvOp],
    budget: usize,
) -> Result<(Vec<u8>, MergeReport), Error> {
    let mut entries = container::parse_csvs(decoded)?;
    let (order, groups) = group_ops_by_table(ops);

    let mut added = 0usize;
    let mut replaced = 0usize;
    let mut row_replaced = 0usize;
    let mut row_appended = 0usize;
    let mut row_group_appended = 0usize;

    for name in &order {
        let idxs = &groups[name];
        let existing = entries.iter().position(|e| e.name.as_str() == *name);
        let (base_bytes, rr, ra, rga) = table_payload(name, idxs, ops, &entries)?;
        row_replaced += rr;
        row_appended += ra;
        row_group_appended += rga;
        let name_field = NameField::from_str(name)?;
        match existing {
            Some(p) => {
                entries[p] = CsvEntry {
                    name: name_field,
                    payload: base_bytes,
                };
                replaced += 1;
            }
            None => {
                entries.push(CsvEntry {
                    name: name_field,
                    payload: base_bytes,
                });
                added += 1;
            }
        }
    }

    let out = container::serialize_csvs(&entries);
    if out.len() > budget {
        return Err(Error::Container(format!(
            "merged csvp container is {} bytes — over the {}-byte budget",
            out.len(),
            budget
        )));
    }
    let file = pack::encode_pack_b(&out);
    let mut report = merge_report(added, replaced, out.len(), file.len());
    report.row_replaced = row_replaced;
    report.row_appended = row_appended;
    report.row_group_appended = row_group_appended;
    Ok((file, report))
}
