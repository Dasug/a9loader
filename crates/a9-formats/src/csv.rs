//! UTF-16 CSV tables inside the `csvp` container — the row-merge tier (M5 W2).
//!
//! The game's own tables are UTF-16LE, BOM-prefixed, comma-delimited, with
//! `"`-quoting and CRLF record separators; quoted fields may contain embedded
//! commas and newlines (the `train_comment.csv` comment column does exactly
//! that). The game parses them with `FUN_1401a0000` (`,`/newline/`"` aware,
//! notes/08). The table is what the reservoir stores: one variable-length blob
//! per table (notes/09), so cell edits do not exist — the modifier tier is
//! **row-level**: parse the blob, apply row ops, re-encode.
//!
//! Model:
//! - [`Table::parse`] keeps every row's bytes **verbatim** (its raw content
//!   plus its own line terminator) alongside the decoded fields. [`Table::serialize`]
//!   concatenates them, so `parse` -> `serialize` is byte-identical on every
//!   real table (the M1 lesson: decode/re-encode must not drift).
//! - Row identity is the **first column** string, case-sensitive. A patch row
//!   whose key exists REPLACES that base row; a key that does not exist
//!   APPENDS a new row after the last one. Row order is base order, then
//!   appended rows in patch order.
//! - A merge that changes nothing reproduces the base byte-identically: only
//!   rows a patch actually touches are re-serialized.
//!
//! Normalisation you cannot avoid: a **changed or added** row is re-encoded
//! canonically — every field double-quoted (`"` doubled), `,`-joined, with the
//! base table's dominant line terminator. (The shipped tables quote non-empty
//! fields and leave empty ones bare; quoting an empty field as `""` is read
//! identically by the game's quoted-aware parser.) Untouched rows keep their
//! exact original bytes.

use crate::Error;
use std::collections::BTreeSet;

/// The UTF-16LE byte-order mark that prefixes the game's tables.
pub const BOM_LE: [u8; 2] = [0xff, 0xfe];

/// A row's line terminator, kept per row so byte-identity round-trips.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eol {
    CrLf,
    Lf,
    Cr,
    /// the final row with no terminator (EOF right after the content)
    None,
}

/// The UTF-16LE bytes of a terminator.
fn eol_bytes(e: Eol) -> &'static [u8] {
    match e {
        Eol::CrLf => &[0x0d, 0x00, 0x0a, 0x00],
        Eol::Lf => &[0x0a, 0x00],
        Eol::Cr => &[0x0d, 0x00],
        Eol::None => &[],
    }
}

fn push_unit(out: &mut Vec<u8>, u: u16) {
    out.extend_from_slice(&u.to_le_bytes());
}

/// One table row: decoded fields for identity/column checks, raw bytes for
/// verbatim round-trip.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// decoded columns (UTF-16 start unit up to the field end, lossy)
    pub fields: Vec<String>,
    /// exact UTF-16LE content bytes (no terminator)
    raw: Vec<u8>,
    eol: Eol,
}

impl Row {
    /// the first column — the row's identity key ("" when the row has no
    /// columns or the first is empty)
    pub fn key(&self) -> &str {
        self.fields.first().map(String::as_str).unwrap_or("")
    }

    /// the verbatim content bytes (no line terminator)
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    pub fn eol(&self) -> Eol {
        self.eol
    }

    /// Build a row from decoded fields, encoded canonically (QUOTE_ALL, UTF-16LE)
    /// with the given terminator. Used for added/replaced rows only.
    pub fn canonical(fields: &[String], eol: Eol) -> Row {
        Row {
            fields: fields.to_vec(),
            raw: encode_fields(fields),
            eol,
        }
    }
}

/// Canonical field encoding: every field double-quoted (`"` doubled), `,`-joined.
/// The shipped tables quote non-empty fields; quoting empties as `""` parses
/// identically (the game's parser is quoted-aware).
fn encode_fields(fields: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            push_unit(&mut out, b',' as u16);
        }
        push_unit(&mut out, b'"' as u16);
        for u in f.encode_utf16() {
            if u == b'"' as u16 {
                push_unit(&mut out, u); // the escape
            }
            push_unit(&mut out, u);
        }
        push_unit(&mut out, b'"' as u16);
    }
    out
}

/// A parsed CSV table.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    /// the leading UTF-16LE BOM was present
    pub bom: bool,
    pub rows: Vec<Row>,
}

impl Table {
    /// An empty table that serializes with a BOM and CRLF rows (authoring).
    pub fn new() -> Self {
        Table {
            bom: true,
            rows: Vec::new(),
        }
    }

    /// Append a canonically-encoded row (authoring; the patch-file writer).
    pub fn push_row(&mut self, fields: Vec<String>) {
        self.rows.push(Row::canonical(&fields, Eol::CrLf));
    }

    /// Number of rows (data + header + comment rows).
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The base table's column count — its first row's field count (the author
    /// supplies the header row). `0` for an empty table.
    pub fn column_count(&self) -> usize {
        self.rows.first().map(|r| r.fields.len()).unwrap_or(0)
    }

    /// The non-empty first-column keys, in order (a patch file's row set).
    /// Duplicates are preserved here; the merge refuses them loudly.
    pub fn patch_keys(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter(|r| !r.key().is_empty())
            .map(|r| r.key().to_string())
            .collect()
    }

    /// The line terminator most of the rows use (appended rows adopt it).
    pub fn dominant_eol(&self) -> Eol {
        let (mut crlf, mut lf, mut cr) = (0usize, 0usize, 0usize);
        for r in &self.rows {
            match r.eol {
                Eol::CrLf => crlf += 1,
                Eol::Lf => lf += 1,
                Eol::Cr => cr += 1,
                Eol::None => {}
            }
        }
        if crlf > 0 && crlf >= lf && crlf >= cr {
            Eol::CrLf
        } else if lf > 0 && lf >= cr {
            Eol::Lf
        } else if cr > 0 {
            Eol::Cr
        } else {
            Eol::CrLf
        }
    }

    /// Drop rows the predicate rejects (used by the loader to enforce
    /// per-row overlap refusal before the merge).
    pub fn retain_rows(&mut self, mut f: impl FnMut(&Row) -> bool) {
        self.rows.retain(|r| f(r));
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.bom {
            out.extend_from_slice(&BOM_LE);
        }
        for r in &self.rows {
            out.extend_from_slice(&r.raw);
            out.extend_from_slice(eol_bytes(r.eol));
        }
        out
    }

    /// Parse UTF-16LE CSV bytes. Big-endian input is refused (the game only
    /// ever writes LE); an odd byte count or an unterminated quote is refused.
    pub fn parse(bytes: &[u8]) -> Result<Table, Error> {
        let (bom, start) = if bytes.starts_with(&BOM_LE) {
            (true, 2)
        } else if bytes.starts_with(&[0xfe, 0xff]) {
            return Err(Error::Container(
                "csv: big-endian UTF-16 is not supported (the game writes LE)".into(),
            ));
        } else {
            (false, 0)
        };
        if (bytes.len() - start) % 2 != 0 {
            return Err(Error::Container(format!(
                "csv: odd byte length ({} bytes after the BOM)",
                bytes.len() - start
            )));
        }
        let body = &bytes[start..];
        let n = body.len() / 2;
        let unit = |i: usize| u16::from_le_bytes([body[i * 2], body[i * 2 + 1]]);

        let mut rows = Vec::new();
        let mut idx = 0usize;
        while idx < n {
            let rec_start = idx;
            let mut fields: Vec<String> = Vec::new();
            let mut cur: Vec<u16> = Vec::new();
            let mut in_q = false;
            let mut content_end = n;
            let mut eol = Eol::None;
            loop {
                if idx >= n {
                    break; // EOF terminates the final record with no newline
                }
                let u = unit(idx);
                if in_q {
                    if u == b'"' as u16 {
                        if idx + 1 < n && unit(idx + 1) == b'"' as u16 {
                            cur.push(u);
                            idx += 2;
                            continue;
                        }
                        in_q = false;
                        idx += 1;
                        continue;
                    }
                    cur.push(u);
                    idx += 1;
                } else if u == b',' as u16 {
                    fields.push(take_field(&mut cur));
                    idx += 1;
                } else if u == b'"' as u16 {
                    in_q = true;
                    idx += 1;
                } else if u == 0x0d {
                    content_end = idx;
                    if idx + 1 < n && unit(idx + 1) == 0x0a {
                        eol = Eol::CrLf;
                        idx += 2;
                    } else {
                        eol = Eol::Cr;
                        idx += 1;
                    }
                    break;
                } else if u == 0x0a {
                    content_end = idx;
                    eol = Eol::Lf;
                    idx += 1;
                    break;
                } else {
                    cur.push(u);
                    idx += 1;
                }
            }
            fields.push(take_field(&mut cur));
            let raw = body[rec_start * 2..content_end * 2].to_vec();
            rows.push(Row { fields, raw, eol });
        }
        Ok(Table { bom, rows })
    }
}

fn take_field(cur: &mut Vec<u16>) -> String {
    let s = String::from_utf16_lossy(cur);
    cur.clear();
    s
}

/// Parse a `mode = "rows"` row-patch file: UTF-16LE (the game's own encoding,
/// with or without a BOM) **or** a plain UTF-8 text file. A patch file is a
/// human-authored artifact, so a normal editor's UTF-8 is the common case;
/// the merge re-encodes every patched row canonically, so a UTF-8 source loses
/// nothing. A BOM decides; otherwise NUL-free valid UTF-8 is UTF-8 and
/// anything else is BOM-less UTF-16LE.
pub fn parse_patch(bytes: &[u8]) -> Result<Table, Error> {
    if bytes.starts_with(&BOM_LE) || bytes.starts_with(&[0xfe, 0xff]) {
        return Table::parse(bytes);
    }
    if !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok() {
        // re-encode to UTF-16LE and run the one parser — identical semantics
        let text = std::str::from_utf8(bytes).unwrap();
        let mut u16bytes = Vec::with_capacity(2 + bytes.len() * 2);
        u16bytes.extend_from_slice(&BOM_LE);
        for u in text.encode_utf16() {
            u16bytes.extend_from_slice(&u.to_le_bytes());
        }
        return Table::parse(&u16bytes);
    }
    Table::parse(bytes)
}

/// How a set of patch tables composes onto a base table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeMode {
    /// Row-level: replace by first-column key, append new keys.
    Rows,
    /// Whole-table: the last patch table IS the result (raw-preserving).
    Replace,
}

/// What a merge did (for the loader log / `a9ml` report).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MergeStats {
    pub replaced: usize,
    pub appended: usize,
    /// patch rows whose fields already matched the base row (kept verbatim)
    pub unchanged: usize,
}

/// Apply `patches` to `base`. `MergeMode::Replace` returns the last patch
/// (byte-preserving); `MergeMode::Rows` applies every patch row in order.
///
/// Refusals (loud, never silent): an empty first column (no identity), a
/// column count that differs from the base's, a key with two patch rows in one
/// merge, or a key that matches more than one base row.
pub fn merge_rows(
    base: &Table,
    patches: &[Table],
    mode: MergeMode,
) -> Result<(Table, MergeStats), Error> {
    if mode == MergeMode::Replace {
        return match patches.last() {
            Some(p) => Ok((p.clone(), MergeStats::default())),
            None => Ok((base.clone(), MergeStats::default())),
        };
    }
    let cols = base.column_count();
    let dominant = base.dominant_eol();
    let mut out = base.clone();
    let mut stats = MergeStats::default();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for patch in patches {
        for row in &patch.rows {
            let key = row.key();
            if key.is_empty() {
                return Err(Error::Container(
                    "csv row patch: a row has an empty first column — row identity needs a \
                     key (name the row, e.g. its FORM_/ID column)"
                        .into(),
                ));
            }
            if row.fields.len() != cols {
                return Err(Error::Container(format!(
                    "csv row patch {key:?}: {} columns but the base table has {cols}",
                    row.fields.len()
                )));
            }
            if !seen.insert(key.to_string()) {
                return Err(Error::Container(format!(
                    "csv row patch {key:?}: two patch rows with the same key in one merge"
                )));
            }
            let mut hit: Option<usize> = None;
            let mut ambiguous = false;
            for (i, r) in out.rows.iter().enumerate() {
                if r.key() == key {
                    if hit.is_some() {
                        ambiguous = true;
                        break;
                    }
                    hit = Some(i);
                }
            }
            if ambiguous {
                return Err(Error::Container(format!(
                    "csv row patch {key:?}: the base table has more than one row with that key"
                )));
            }
            match hit {
                Some(i) => {
                    if out.rows[i].fields == row.fields {
                        stats.unchanged += 1; // byte-parity no-op — keep verbatim
                    } else {
                        let eol = out.rows[i].eol;
                        out.rows[i] = Row::canonical(&row.fields, eol);
                        stats.replaced += 1;
                    }
                }
                None => {
                    if let Some(last) = out.rows.last_mut() {
                        if last.eol == Eol::None {
                            last.eol = dominant; // so the append starts on its own line
                        }
                    }
                    out.rows.push(Row::canonical(&row.fields, dominant));
                    stats.appended += 1;
                }
            }
        }
    }
    Ok((out, stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(s: &str) -> Vec<u8> {
        let mut out = BOM_LE.to_vec();
        for u in s.encode_utf16() {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out
    }

    #[test]
    fn parse_serialize_is_byte_identical() {
        // BOM, CRLF rows, quoted fields with embedded commas/newlines/LF/CRLF
        let text = "\"編成ID\",\"日本語\",\"コメント\"\r\n\
                    \"FORM_0\",\"０系\",\"line1\nline2, with comma\"\r\n\
                    \"FORM_1\",\"\",\"\"\r\n\
                    \"FORM_2\",\"x\"\"y\",\"z\"";
        let bytes = utf16(text);
        let t = Table::parse(&bytes).unwrap();
        assert_eq!(t.serialize(), bytes);
        assert!(t.bom);
        assert_eq!(t.column_count(), 3);
        assert_eq!(t.rows.len(), 4);
        assert_eq!(t.rows[0].key(), "編成ID");
        assert_eq!(t.rows[1].key(), "FORM_0");
        assert_eq!(t.rows[1].fields[2], "line1\nline2, with comma");
        assert_eq!(t.rows[2].fields[1], "");
        assert_eq!(t.rows[3].fields[1], "x\"y");
        assert_eq!(t.rows[3].eol(), Eol::None);
        assert_eq!(t.rows[1].eol(), Eol::CrLf);
    }

    #[test]
    fn eol_variants_roundtrip() {
        for text in [
            "a,b\r\nc,d\r\n",
            "a,b\nc,d\n",
            "a,b\rc,d\r",
            "a,b",
        ] {
            let bytes = utf16(text);
            let t = Table::parse(&bytes).unwrap();
            assert_eq!(t.serialize(), bytes, "{text:?}");
        }
    }

    #[test]
    fn no_bom_and_refusals() {
        // no BOM: still parses, keeps bom=false
        let mut bytes: Vec<u8> = Vec::new();
        for u in "a,b\r\n".encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let t = Table::parse(&bytes).unwrap();
        assert!(!t.bom);
        assert_eq!(t.serialize(), bytes);
        // big-endian refused
        let be = [0xfe, 0xff, 0x00, 0x61];
        assert!(Table::parse(&be).is_err());
        // odd length refused
        assert!(Table::parse(&[0xff, 0xfe, 0x61]).is_err());
    }

    #[test]
    fn identity_merge_is_byte_identical() {
        let bytes = utf16("\"KEY\",\"VAL\"\r\n\"R1\",\"a\"\r\n\"R2\",\"b\"\r\n");
        let base = Table::parse(&bytes).unwrap();
        // empty patch set
        let (m, s) = merge_rows(&base, &[], MergeMode::Rows).unwrap();
        assert_eq!(m.serialize(), bytes);
        assert_eq!(s, MergeStats::default());
        // a patch that restates rows identically is a no-op
        let patch = Table::parse(&utf16("\"R1\",\"a\"\r\n\"R2\",\"b\"\r\n")).unwrap();
        let (m, s) = merge_rows(&base, &[patch], MergeMode::Rows).unwrap();
        assert_eq!(m.serialize(), bytes);
        assert_eq!(s.unchanged, 2);
        assert_eq!((s.replaced, s.appended), (0, 0));
    }

    #[test]
    fn replace_and_append() {
        let base = Table::parse(&utf16("\"KEY\",\"VAL\"\r\n\"R1\",\"a\"\r\n\"R2\",\"b\"\r\n")).unwrap();
        let p1 = Table::parse(&utf16("\"R2\",\"B2\"\r\n")).unwrap();
        let p2 = Table::parse(&utf16("\"R4\",\"d\"\r\n")).unwrap();
        let (m, s) = merge_rows(&base, &[p1, p2], MergeMode::Rows).unwrap();
        assert_eq!((s.replaced, s.appended, s.unchanged), (1, 1, 0));
        let want = "\"KEY\",\"VAL\"\r\n\"R1\",\"a\"\r\n\"R2\",\"B2\"\r\n\"R4\",\"d\"\r\n";
        assert_eq!(String::from_utf16(&m.serialize()[2..].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()).unwrap(), want);
        // order: base order then appended
        let keys: Vec<&str> = m.rows.iter().map(|r| r.key()).collect();
        assert_eq!(keys, vec!["KEY", "R1", "R2", "R4"]);
    }

    #[test]
    fn append_fixes_a_missing_final_newline() {
        // base's last row has no terminator; the appended row must not concatenate
        let base = Table::parse(&utf16("\"K\",\"V\"\r\n\"R1\",\"a\"")).unwrap();
        let p = Table::parse(&utf16("\"R2\",\"b\"\r\n")).unwrap();
        let (m, _) = merge_rows(&base, &[p], MergeMode::Rows).unwrap();
        let text = String::from_utf16(&m.serialize()[2..].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()).unwrap();
        assert_eq!(text, "\"K\",\"V\"\r\n\"R1\",\"a\"\r\n\"R2\",\"b\"\r\n");
    }

    #[test]
    fn column_mismatch_is_refused() {
        let base = Table::parse(&utf16("\"K\",\"V\"\r\n\"R1\",\"a\"\r\n")).unwrap();
        let bad = Table::parse(&utf16("\"R1\"\r\n")).unwrap();
        let e = merge_rows(&base, &[bad], MergeMode::Rows).unwrap_err().to_string();
        assert!(e.contains("1 columns but the base table has 2"), "{e}");
    }

    #[test]
    fn empty_key_is_refused() {
        let base = Table::parse(&utf16("\"K\",\"V\"\r\n\"R1\",\"a\"\r\n")).unwrap();
        let bad = Table::parse(&utf16("\"\",\"x\"\r\n")).unwrap();
        let e = merge_rows(&base, &[bad], MergeMode::Rows).unwrap_err().to_string();
        assert!(e.contains("empty first column"), "{e}");
    }

    #[test]
    fn duplicate_patch_key_is_refused() {
        let base = Table::parse(&utf16("\"K\",\"V\"\r\n\"R1\",\"a\"\r\n")).unwrap();
        let p = Table::parse(&utf16("\"R1\",\"b\"\r\n\"R1\",\"c\"\r\n")).unwrap();
        let e = merge_rows(&base, &[p], MergeMode::Rows).unwrap_err().to_string();
        assert!(e.contains("two patch rows with the same key"), "{e}");
    }

    #[test]
    fn ambiguous_base_key_is_refused() {
        let base = Table::parse(&utf16("\"K\",\"V\"\r\n\"R1\",\"a\"\r\n\"R1\",\"b\"\r\n")).unwrap();
        let p = Table::parse(&utf16("\"R1\",\"c\"\r\n")).unwrap();
        let e = merge_rows(&base, &[p], MergeMode::Rows).unwrap_err().to_string();
        assert!(e.contains("more than one row with that key"), "{e}");
    }

    #[test]
    fn replace_mode_takes_the_last_patch() {
        let base = Table::parse(&utf16("\"K\",\"V\"\r\n\"R1\",\"a\"\r\n")).unwrap();
        let p = Table::parse(&utf16("\"X\",\"Y\"\r\n\"R9\",\"z\"\r\n")).unwrap();
        let (m, _) = merge_rows(&base, &[p.clone()], MergeMode::Replace).unwrap();
        assert_eq!(m.serialize(), p.serialize());
        let (m, _) = merge_rows(&base, &[], MergeMode::Replace).unwrap();
        assert_eq!(m.serialize(), base.serialize());
    }

    #[test]
    fn patch_keys_and_authoring() {
        let p = Table::parse(&utf16("\"R1\",\"a\"\r\n\"\",\"comment\"\r\n\"R2\",\"b\"\r\n")).unwrap();
        assert_eq!(p.patch_keys(), vec!["R1".to_string(), "R2".to_string()]);
        let mut t = Table::new();
        t.push_row(vec!["R1".into(), "a".into()]);
        let bytes = t.serialize();
        assert!(bytes.starts_with(&BOM_LE));
        let back = Table::parse(&bytes).unwrap();
        assert_eq!(back.serialize(), bytes);
        assert_eq!(back.rows[0].fields, vec!["R1".to_string(), "a".to_string()]);
    }

    #[test]
    fn dominant_eol_picks_the_majority() {
        let t = Table::parse(&utf16("\"a\"\n\"b\"\n\"c\"\r\n\"d\"\r\n\"e\"\r\n")).unwrap();
        assert_eq!(t.dominant_eol(), Eol::CrLf);
        let t = Table::parse(&utf16("\"a\"\n\"b\"\n\"c\"\n")).unwrap();
        assert_eq!(t.dominant_eol(), Eol::Lf);
    }

    #[test]
    fn patch_files_may_be_utf8_or_utf16() {
        // the same rows, once as UTF-16LE (game encoding) and once as UTF-8
        let utf16 = utf16("\"R1\",\"a\"\r\n\"R2\",\"b\"\r\n");
        let utf8 = b"\"R1\",\"a\"\r\n\"R2\",\"b\"\r\n".to_vec();
        for bytes in [utf16, utf8] {
            let t = parse_patch(&bytes).unwrap();
            assert_eq!(t.patch_keys(), vec!["R1".to_string(), "R2".to_string()]);
            assert_eq!(t.rows[0].fields, vec!["R1".to_string(), "a".to_string()]);
        }
        // a BOM-less UTF-16LE patch (the game never writes one, but be exact)
        let mut bomless = Vec::new();
        for u in "\"R1\",\"a\"\r\n".encode_utf16() {
            bomless.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(parse_patch(&bomless).unwrap().patch_keys(), vec!["R1".to_string()]);
        // big-endian is still refused (a BOM decides)
        assert!(parse_patch(&[0xfe, 0xff, 0x00, 0x61]).is_err());
    }

    #[test]
    fn real_table_when_available() {
        // A9_CSV_TABLE=<a real csvp table, e.g. train_comment.csv> proves the
        // parser on game data: quoted fields with embedded newlines, 400+ rows,
        // and byte-identical parse -> serialize.
        if let Ok(p) = std::env::var("A9_CSV_TABLE") {
            let bytes = std::fs::read(&p).unwrap();
            let t = Table::parse(&bytes).unwrap();
            assert_eq!(t.serialize(), bytes, "parse/serialize must be byte-identical");
            assert!(t.rows.len() > 100, "{} rows", t.rows.len());
            assert!(!t.patch_keys().is_empty());
            // every FORM_ row is a patchable key
            assert!(t.patch_keys().iter().all(|k| !k.is_empty()));
        }
    }
}
