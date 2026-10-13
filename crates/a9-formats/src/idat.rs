//! `i.dat` — the master asset index, port of corpus `tools/i_parse.py`
//! (verified byte-identical round-trip on the demo build; loader
//! `FUN_140314fa0`, entry parse `FUN_140314e60`).
//!
//! Decoded layout:
//!
//! ```text
//! 0x00  UTF-16 "adzpidx00" magic (9 units incl. NUL, 0x12 bytes) — invariant
//! 0x12  6 bytes of build-specific padding: demo build = zeros,
//!       international Steam build = UTF-16 "000" (30 00 30 00 30 00)
//! 0x18  u32 LE entry count
//! 0x1c  entries…
//!
//! entry: [u16 size][u16 nameLen][name UTF-16, nameLen units incl. NUL]
//!        [u16 dirLen][dir UTF-16, dirLen units incl. NUL][9-byte tail]
//!        (tail = u32 v1 + u8 v2 + the 4 bytes the loader reads unaligned at +5)
//! ```
//!
//! The loader advances by the `size` field, so the walk self-checks; the
//! serializer keeps each entry's 9-byte tail and the header padding verbatim —
//! that is what makes the round-trip byte-identical across builds (this is
//! exactly where `i_parse.py`'s demo-only header constant breaks on the
//! Steam file: its padding differs, and hard-coding it corrupts the output).

use crate::Error;

/// The invariant magic: UTF-16 `adzpidx00` (with trailing NUL) — 0x12 bytes.
pub const MAGIC: [u8; 0x12] = [
    0x61, 0x00, 0x64, 0x00, 0x7a, 0x00, 0x70, 0x00, 0x69, 0x00, 0x64, 0x00, 0x78, 0x00, //
    0x30, 0x00, 0x30, 0x00,
];

/// Demo-build header padding (bytes `0x12..0x18`) — six zero bytes.
pub const PAD_DEMO: [u8; 6] = [0; 6];
/// International Steam build padding — UTF-16 `"000"`.
pub const PAD_STEAM: [u8; 6] = [0x30, 0x00, 0x30, 0x00, 0x30, 0x00];

/// A parsed `i.dat`: build-specific header padding (kept verbatim) + rows.
#[derive(Clone, Debug, PartialEq)]
pub struct Idat {
    pub pad: [u8; 6],
    pub entries: Vec<Entry>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub dir: String,
    /// v1 (u32), v2 (u8) and the 4-byte unaligned tail — kept verbatim
    pub tail: [u8; 9],
}

impl Entry {
    pub fn v1(&self) -> u32 {
        u32::from_le_bytes(self.tail[0..4].try_into().unwrap())
    }
    pub fn v2(&self) -> u8 {
        self.tail[4]
    }
    /// the u32 the loader reads unaligned at tail+5
    pub fn v3(&self) -> u32 {
        u32::from_le_bytes(self.tail[5..9].try_into().unwrap())
    }
}

/// The walked size of one entry record: `[u16 size][u16 nameLen][name UTF-16
/// + NUL][u16 dirLen][dir UTF-16 + NUL][9-byte tail]`. Shared by [`serialize`]
/// (a parsed [`Entry`]) and [`NewRow::to_bytes`] (a synthesized row) so the
/// two renderings cannot drift.
fn record_size(name: &str, dir: &str) -> usize {
    let nl = name.encode_utf16().count() + 1;
    let dl = dir.encode_utf16().count() + 1;
    4 + nl * 2 + 2 + dl * 2 + 9
}

/// Append one entry record (the exact byte layout the loader walks: it reads
/// the leading `size` and advances by it).
fn write_record(out: &mut Vec<u8>, name: &str, dir: &str, tail: &[u8; 9]) {
    let nl = name.encode_utf16().count() + 1;
    let dl = dir.encode_utf16().count() + 1;
    out.extend_from_slice(&(record_size(name, dir) as u16).to_le_bytes());
    out.extend_from_slice(&(nl as u16).to_le_bytes());
    for u in name.encode_utf16() {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes()); // NUL terminator
    out.extend_from_slice(&(dl as u16).to_le_bytes());
    for u in dir.encode_utf16() {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes()); // NUL terminator
    out.extend_from_slice(tail);
}

/// A row to add or replace in a parsed `i.dat` (M5 W1 `[[asset]]`).
///
/// The tail is *synthesized* from these fields — a new row has no verbatim
/// bytes to preserve. Field meanings are the loader's own
/// (notes/05 lines 148-161): `len` = bytes consumers get (node+0x48; for a
/// `.x` this includes the 12-byte `ADX0` chunk header), `flag` = node+0x4c
/// (0 in every shipped row), `off` = offset into the current pack's decoded
/// buffer (node+0x50).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewRow {
    pub name: String,
    pub dir: String,
    pub len: u32,
    pub flag: u8,
    pub off: u32,
}

impl NewRow {
    /// The 9-byte row tail: `[len u32][flag u8][off u32]`.
    fn tail(&self) -> [u8; 9] {
        let mut t = [0u8; 9];
        t[0..4].copy_from_slice(&self.len.to_le_bytes());
        t[4] = self.flag;
        t[5..9].copy_from_slice(&self.off.to_le_bytes());
        t
    }

    /// This row as one serialized entry record — byte-identical to what
    /// [`Idat::serialize`] emits for an equivalent [`Entry`]. The M5 tailgate
    /// stub appends these bytes to the decoded `i.dat` image and bumps the
    /// entry count, which is exactly the merged-table layout minus the
    /// untouched vanilla prefix.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(record_size(&self.name, &self.dir));
        write_record(&mut out, &self.name, &self.dir, &self.tail());
        out
    }
}

/// Render `rows` as the concatenated entry records [`NewRow::to_bytes`]
/// produces, in order — the tailgate's append payload for `i.dat`.
pub fn render_rows(rows: &[NewRow]) -> Vec<u8> {
    let mut out = Vec::new();
    for r in rows {
        out.extend_from_slice(&r.to_bytes());
    }
    out
}

/// Bump the `u32` entry count inside a decoded `i.dat` IMAGE in place (the
/// count sits at `image[0x18]`, entries run from `image[0x1c]`; the loader
/// reads the count right after the decode). The tailgate's count half of the
/// row append; `Err` if the image is too short to hold the count word.
pub fn bump_count(image: &mut [u8], add: u32) -> Result<(), Error> {
    if image.len() < 0x1c {
        return Err(Error::Idat(format!(
            "decoded i.dat image is {} bytes — too short for the count word",
            image.len()
        )));
    }
    let c = u32::from_le_bytes(image[0x18..0x1c].try_into().unwrap());
    image[0x18..0x1c].copy_from_slice(&c.wrapping_add(add).to_le_bytes());
    Ok(())
}

impl Idat {
    /// The row with this bare name, if present.
    pub fn row(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Add rows to the parsed table. A name that already exists **replaces**
    /// that entry's tail in place (its position and `name`/`dir` are kept —
    /// this is the `override_vanilla` case; the *caller* decides whether a
    /// collision with a shipped row is allowed); a name that does not exist
    /// **appends** a row. The header pad and every untouched entry's 9-byte
    /// tail stay verbatim, so a merge that adds nothing serializes
    /// byte-identically to its input.
    ///
    /// Two rows in `rows` carrying the same name are refused — that is a
    /// caller bug, not a collision policy decision.
    pub fn add_rows(&mut self, rows: &[NewRow]) -> Result<(), Error> {
        let mut seen = std::collections::BTreeSet::new();
        for r in rows {
            if r.name.is_empty() || r.name.contains('\0') {
                return Err(Error::Idat(format!(
                    "new row name must be a nonempty plain name, got {:?}",
                    r.name
                )));
            }
            if r.dir.contains('\0') {
                return Err(Error::Idat(format!(
                    "new row {:?}: dir must not contain NUL",
                    r.name
                )));
            }
            if !seen.insert(r.name.as_str()) {
                return Err(Error::Idat(format!(
                    "two new rows named {:?} in one merge",
                    r.name
                )));
            }
        }
        for r in rows {
            match self.entries.iter_mut().find(|e| e.name == r.name) {
                Some(e) => e.tail = r.tail(), // replace: keep name/dir/position
                None => self.entries.push(Entry {
                    name: r.name.clone(),
                    dir: r.dir.clone(),
                    tail: r.tail(),
                }),
            }
        }
        Ok(())
    }
}

fn read_str(data: &[u8], pos: usize, units: usize) -> Result<String, Error> {
    let end = pos + units * 2;
    if end > data.len() {
        return Err(Error::Idat(format!(
            "string at {pos:#x} overruns the buffer"
        )));
    }
    let u16s: Vec<u16> = data[pos..end]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    Ok(String::from_utf16_lossy(&u16s))
}

pub fn parse(decoded: &[u8]) -> Result<Idat, Error> {
    if decoded.len() < 0x1c || decoded[..0x12] != MAGIC {
        return Err(Error::Idat("bad magic".into()));
    }
    let pad: [u8; 6] = decoded[0x12..0x18].try_into().unwrap();
    let count = u32::from_le_bytes(decoded[0x18..0x1c].try_into().unwrap()) as usize;
    let mut pos = 0x1cusize;
    let mut entries = Vec::with_capacity(count.min(1 << 16));
    for idx in 0..count {
        if pos + 4 > decoded.len() {
            return Err(Error::Idat(format!("entry {idx}: header at {pos:#x} overruns")));
        }
        let size = u16::from_le_bytes(decoded[pos..pos + 2].try_into().unwrap()) as usize;
        let name_len = u16::from_le_bytes(decoded[pos + 2..pos + 4].try_into().unwrap()) as usize;
        let npos = pos + 4 + name_len * 2;
        if npos > decoded.len() {
            return Err(Error::Idat(format!("entry {idx}: name overruns")));
        }
        let name = read_str(decoded, pos + 4, name_len)?;
        if npos + 2 > decoded.len() {
            return Err(Error::Idat(format!("entry {idx}: dirLen overruns")));
        }
        let dir_len = u16::from_le_bytes(decoded[npos..npos + 2].try_into().unwrap()) as usize;
        let dpos = npos + 2 + dir_len * 2;
        if dpos > decoded.len() {
            return Err(Error::Idat(format!("entry {idx}: dir overruns")));
        }
        let dir = read_str(decoded, npos + 2, dir_len)?;
        let end = dpos + 9;
        if end > decoded.len() {
            return Err(Error::Idat(format!("entry {idx}: tail overruns")));
        }
        if pos + size != end {
            return Err(Error::Idat(format!(
                "entry {idx}: size field {size} != walked size {} (buffer corrupt)",
                end - pos
            )));
        }
        entries.push(Entry {
            name,
            dir,
            tail: decoded[dpos..end].try_into().unwrap(),
        });
        pos = end;
    }
    if pos != decoded.len() {
        return Err(Error::Idat(format!(
            "{} trailing bytes after {count} entries",
            decoded.len() - pos
        )));
    }
    Ok(Idat { pad, entries })
}

pub fn serialize(idat: &Idat) -> Vec<u8> {
    let entries = &idat.entries;
    let mut out = Vec::with_capacity(0x1c + entries.len() * 40);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&idat.pad);
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        write_record(&mut out, &e.name, &e.dir, &e.tail);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, dir: &str, tail: [u8; 9]) -> Entry {
        Entry { name: name.into(), dir: dir.into(), tail }
    }

    fn idat(pad: [u8; 6], entries: Vec<Entry>) -> Idat {
        Idat { pad, entries }
    }

    #[test]
    fn roundtrip_both_build_paddings() {
        for pad in [PAD_DEMO, PAD_STEAM] {
            let x = idat(
                pad,
                vec![
                    entry("s_orm_02_f.dds", "dai", [1, 0, 0, 0, 0, 9, 0, 0, 0]),
                    entry("previewtexture.dir", "rail\\_rlh", [0xff; 9]),
                    entry("x.x", "", [0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00]),
                ],
            );
            let ser = serialize(&x);
            assert_eq!(parse(&ser).unwrap(), x, "pad = {pad:?}");
            assert_eq!(serialize(&parse(&ser).unwrap()), ser);
            assert_eq!(&ser[0x12..0x18], &pad);
        }
    }

    #[test]
    fn size_field_matches_walked_layout() {
        let x = idat(PAD_STEAM, vec![entry("abc", "de", [7; 9])]);
        let ser = serialize(&x);
        let size = u16::from_le_bytes(ser[0x1c..0x1c + 2].try_into().unwrap()) as usize;
        // 4 + (3+1)*2 + 2 + (2+1)*2 + 9 = 29
        assert_eq!(size, 29);
        assert_eq!(ser.len(), 0x1c + 29);
    }

    #[test]
    fn rejects_bad_magic_and_corruption() {
        assert!(matches!(parse(&[]), Err(Error::Idat(_))));
        let ser = serialize(&idat(PAD_DEMO, vec![entry("a", "b", [0; 9])]));
        let mut bad = ser.clone();
        bad[0] ^= 0xff;
        assert!(matches!(parse(&bad), Err(Error::Idat(_))));
        let mut trailing = ser.clone();
        trailing.push(0);
        assert!(matches!(parse(&trailing), Err(Error::Idat(_))));
        // corrupt the size field so it disagrees with the walk
        let mut sized = ser;
        let size_pos = 0x1c;
        sized[size_pos..size_pos + 2].copy_from_slice(&30u16.to_le_bytes());
        assert!(matches!(parse(&sized), Err(Error::Idat(_))));
    }

    #[test]
    fn entry_accessors() {
        let e = entry("abc", "d", [0x39, 0x30, 0x00, 0x00, 0x07, 0x09, 0x00, 0x00, 0x00]);
        assert_eq!(e.v1(), 0x0000_3039);
        assert_eq!(e.v2(), 7);
        assert_eq!(e.v3(), 9);
    }

    fn new_row(name: &str, dir: &str, len: u32, flag: u8, off: u32) -> NewRow {
        NewRow { name: name.into(), dir: dir.into(), len, flag, off }
    }

    #[test]
    fn add_rows_is_identity_when_empty() {
        let x = idat(
            PAD_STEAM,
            vec![
                entry("_t_a.x", "train\\a", [1, 0, 0, 0, 0, 9, 0, 0, 0]),
                entry("previewtexture.dir", "rail\\_rlh", [0xff; 9]),
            ],
        );
        let before = serialize(&x);
        let mut y = x.clone();
        y.add_rows(&[]).unwrap();
        assert_eq!(serialize(&y), before, "no rows must round-trip byte-identically");
        assert_eq!(y, x);
    }

    #[test]
    fn add_rows_appends_new_and_synthesizes_the_tail() {
        let mut x = idat(PAD_STEAM, vec![entry("_t_old.x", "train\\old", [7; 9])]);
        let old_tail = x.entries[0].tail;
        x.add_rows(&[new_row("_t_new.x", "train\\new", 12_345, 0, 107_696_939)])
            .unwrap();
        assert_eq!(x.entries.len(), 2);
        // pre-existing row untouched, pad untouched
        assert_eq!(x.entries[0].tail, old_tail);
        assert_eq!(x.pad, PAD_STEAM);
        let n = &x.entries[1];
        assert_eq!(n.name, "_t_new.x");
        assert_eq!(n.dir, "train\\new");
        assert_eq!(n.v1(), 12_345);
        assert_eq!(n.v2(), 0);
        assert_eq!(n.v3(), 107_696_939);
        assert_eq!(n.tail, {
            let mut t = [0u8; 9];
            t[0..4].copy_from_slice(&12_345u32.to_le_bytes());
            t[5..9].copy_from_slice(&107_696_939u32.to_le_bytes());
            t
        });
        // the appended table parses back byte-exactly
        let ser = serialize(&x);
        assert_eq!(parse(&ser).unwrap(), x);
        assert!(x.row("_t_new.x").is_some());
        assert!(x.row("nope").is_none());
    }

    #[test]
    fn add_rows_replaces_an_existing_tail_in_place() {
        let mut x = idat(
            PAD_STEAM,
            vec![
                entry("_t_van_a.x", "train\\van", [37, 0, 0, 0, 0, 0, 0, 0, 0]),
                entry("_t_keep.x", "train\\keep", [1, 0, 0, 0, 0, 2, 0, 0, 0]),
            ],
        );
        x.add_rows(&[new_row("_t_van_a.x", "ignored\\dir", 999, 0, 6_000)])
            .unwrap();
        assert_eq!(x.entries.len(), 2, "replace does not add a row");
        let e = &x.entries[0];
        assert_eq!(e.name, "_t_van_a.x", "name preserved");
        assert_eq!(e.dir, "train\\van", "dir preserved on replace");
        assert_eq!(e.v1(), 999);
        assert_eq!(e.v3(), 6_000);
        assert_eq!(x.entries[1].tail, [1, 0, 0, 0, 0, 2, 0, 0, 0]);
    }

    #[test]
    fn add_rows_refuses_duplicate_names_in_one_call() {
        let mut x = idat(PAD_DEMO, vec![]);
        let err = x
            .add_rows(&[
                new_row("dup.x", "d", 1, 0, 0),
                new_row("dup.x", "d", 2, 0, 0),
            ])
            .unwrap_err();
        assert!(err.to_string().contains("two new rows named"), "{err}");
        assert!(x.entries.is_empty());
        // empty / NUL names are refused too
        assert!(x.add_rows(&[new_row("", "d", 1, 0, 0)]).is_err());
        assert!(x.add_rows(&[new_row("a\0b", "d", 1, 0, 0)]).is_err());
    }

    #[test]
    fn new_row_render_matches_the_serializer() {
        // a row only ever appended (off = |V|) renders EXACTLY like the
        // equivalent Entry does in serialize()
        let r = new_row("_t_new.x", "train\\new", 12_345, 0, 64);
        let eq = entry("_t_new.x", "train\\new", r.tail());
        assert_eq!(r.to_bytes(), serialize(&idat(PAD_DEMO, vec![eq]))[0x1c..].to_vec());
        // and the full-image append path round-trips
        let mut image = serialize(&idat(PAD_STEAM, vec![entry("_t_van_a.x", "train\\van", [37, 0, 0, 0, 0, 0, 0, 0, 0])]));
        let appended = render_rows(&[r.clone()]);
        let vanilla_len = image.len();
        bump_count(&mut image, 1).unwrap();
        image.extend_from_slice(&appended);
        let back = parse(&image).unwrap();
        assert_eq!(back.entries.len(), 2);
        assert_eq!(back.entries[1].name, "_t_new.x");
        assert_eq!(back.entries[1].dir, "train\\new");
        assert_eq!(back.entries[1].v1(), 12_345);
        assert_eq!(back.entries[1].v3(), 64);
        // the record starts exactly at the vanilla image's end
        assert_eq!(image.len() - vanilla_len, appended.len());
        assert_eq!(&image[vanilla_len..], &appended[..]);
    }

    /// The M5 tailgate's override mechanism: a duplicate row APPENDED to a
    /// decoded i.dat image (no in-place rewrite). The game's master map is a
    /// std::map operator[]-style insert-or-find — `FUN_140315840` returns the
    /// EXISTING node on a name hit and the loader overwrites its value fields
    /// unconditionally — so a LATER duplicate WINS. This test renders the
    /// tailgate's exact bytes and replays that last-wins walk.
    #[test]
    fn tailgate_duplicate_row_is_last_wins() {
        // the shipped image carries `x` at (off 100, len 10)
        let mut image = serialize(&idat(
            PAD_STEAM,
            vec![entry("x", "d", {
                let mut t = [0u8; 9];
                t[0..4].copy_from_slice(&10u32.to_le_bytes());
                t[5..9].copy_from_slice(&100u32.to_le_bytes());
                t
            })],
        ));
        let vanilla_len = image.len();
        image.extend_from_slice(&render_rows(&[new_row("x", "d", 20, 0, 200)]));
        bump_count(&mut image, 1).unwrap();

        // the image now parses as two rows (the duplicate included)
        let back = parse(&image).unwrap();
        assert_eq!(back.entries.len(), 2);
        assert_eq!(image.len() - vanilla_len, new_row("x", "d", 20, 0, 200).to_bytes().len());

        // replay the loader: insert-or-find, value fields overwritten each time
        let mut map: std::collections::BTreeMap<&str, (u32, u32)> =
            std::collections::BTreeMap::new();
        for e in &back.entries {
            map.insert(e.name.as_str(), (e.v3(), e.v1())); // last wins
        }
        assert_eq!(map["x"], (200, 20), "the appended duplicate must win");
    }

    #[test]
    fn render_rows_is_the_concatenation_in_order() {        let a = new_row("a.x", "d", 1, 0, 10);
        let b = new_row("b.x", "d", 2, 0, 20);
        let blob = render_rows(&[a.clone(), b.clone()]);
        let mut want = a.to_bytes();
        want.extend_from_slice(&b.to_bytes());
        assert_eq!(blob, want);
        assert_eq!(render_rows(&[]), Vec::<u8>::new());
    }

    #[test]
    fn bump_count_wraps_and_rejects_short_images() {
        let mut img = vec![0u8; 0x1c];
        img[0x18..0x1c].copy_from_slice(&7u32.to_le_bytes());
        bump_count(&mut img, 3).unwrap();
        assert_eq!(u32::from_le_bytes(img[0x18..0x1c].try_into().unwrap()), 10);
        bump_count(&mut img, u32::MAX).unwrap();
        assert_eq!(u32::from_le_bytes(img[0x18..0x1c].try_into().unwrap()), 9);
        assert!(bump_count(&mut [0u8; 0x1b], 1).is_err());
    }
}
