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
    fn size(&self) -> usize {
        let nl = self.name.encode_utf16().count() + 1;
        let dl = self.dir.encode_utf16().count() + 1;
        4 + nl * 2 + 2 + dl * 2 + 9
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
        let size = e.size();
        let nl = e.name.encode_utf16().count() + 1;
        let dl = e.dir.encode_utf16().count() + 1;
        out.extend_from_slice(&(size as u16).to_le_bytes());
        out.extend_from_slice(&(nl as u16).to_le_bytes());
        for u in e.name.encode_utf16() {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes()); // NUL terminator
        out.extend_from_slice(&(dl as u16).to_le_bytes());
        for u in e.dir.encode_utf16() {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes()); // NUL terminator
        out.extend_from_slice(&e.tail);
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
}
