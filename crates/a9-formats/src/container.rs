//! Family-B container shapes — the stored-name fields, output sizes and
//! per-entry payloads of the family-B containers (me/mt*/mtentr0/mtcutp0).
//! (image packs) and `tools/build_csvp_ascii.py` (the 0x204 CSV variant used
//! by `csvp`/`sdp`/`csdp`).
//!
//! Image pack: `[u32 count]` then per entry
//! `[0x200 UTF-16 name field][u32 w][u32 h][u32 stored][payload]`.
//!
//! CSV pack: `[u32 count]` then per entry
//! `[0x200 UTF-16 name field][u32 len][data]`.
//!
//! **Name fields are kept verbatim.** ARTDINK's own serializer leaves garbage
//! in the padding after the NUL terminator (proven on the original packs:
//! leftover filename fragments in `mttkmu0`'s and `sdp`'s fields, cleanly
//! zeroed in `me0`'s), so decoding to a string and re-encoding is *not*
//! lossless on original packs. [`NameField`] stores the raw bytes, decodes
//! only for display, and serializes exactly what it parsed — parse→serialize
//! is byte-identical on every real pack. Builder-style construction
//! ([`NameField::from_str`]) zero-pads like the reference pack builders.
//!
//! Both walks must consume the decoded buffer **exactly** — the structural
//! sanity check that catches decoder bugs, run on every
//! real pack by `a9ml verify`.

use crate::Error;

/// Fixed-width UTF-16 name field of both container variants.
pub const NAME_FIELD: usize = 0x200;

/// A raw 0x200-byte UTF-16 name field — verbatim round-trip, decoded view.
#[derive(Clone, Debug, PartialEq)]
pub struct NameField([u8; NAME_FIELD]);

impl NameField {
    /// Builder construction: UTF-16 + NUL + zero padding — the exact
    /// field bytes the reference pack builders
    /// `build_csvp_ascii.py`) produce.
    pub fn from_str(name: &str) -> Result<Self, Error> {
        if name.contains('\0') {
            return Err(Error::Container(format!("name contains NUL: {name:?}")));
        }
        let mut field = [0u8; NAME_FIELD];
        let mut off = 0usize;
        for u in name.encode_utf16() {
            if off + 2 > NAME_FIELD - 2 {
                // needs room for the NUL terminator too
                return Err(Error::NameTooLong { len: off + 2 });
            }
            field[off..off + 2].copy_from_slice(&u.to_le_bytes());
            off += 2;
        }
        Ok(Self(field)) // NUL + zero padding already in place
    }

    /// Verbatim from a parsed field.
    pub fn from_field(bytes: &[u8; NAME_FIELD]) -> Self {
        Self(*bytes)
    }

    /// The decoded name: UTF-16 up to the first NUL (lossy). This is the
    /// display/lookup name; it is not what serializes.
    pub fn as_str(&self) -> String {
        let units: Vec<u16> = self
            .0
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        String::from_utf16_lossy(&units)
    }

    pub fn field(&self) -> &[u8; NAME_FIELD] {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImageEntry {
    pub name: NameField,
    pub width: u32,
    pub height: u32,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CsvEntry {
    pub name: NameField,
    pub payload: Vec<u8>,
}

/// `(w, h)` from a DDS header, else `(0, 0)` — mirrors `dds_dims()`
/// (dwHeight at +12, dwWidth at +16).
pub fn dds_dims(payload: &[u8]) -> (u32, u32) {
    if payload.len() >= 20 && payload[..4] == *b"DDS " {
        let h = u32::from_le_bytes(payload[12..16].try_into().unwrap());
        let w = u32::from_le_bytes(payload[16..20].try_into().unwrap());
        (w, h)
    } else {
        (0, 0)
    }
}

pub fn parse_images(buf: &[u8]) -> Result<Vec<ImageEntry>, Error> {
    if buf.len() < 4 {
        return Err(Error::TooShort(buf.len()));
    }
    let count = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
    let mut off = 4usize;
    let mut out = Vec::with_capacity(count.min(4096));
    for idx in 0..count {
        let hdr = NAME_FIELD + 12;
        if off + hdr > buf.len() {
            return Err(Error::Container(format!(
                "image entry {idx}: header at {off:#x} overruns the buffer"
            )));
        }
        let name = NameField::from_field(buf[off..off + NAME_FIELD].try_into().unwrap());
        let w = u32::from_le_bytes(buf[off + NAME_FIELD..][..4].try_into().unwrap());
        let h = u32::from_le_bytes(buf[off + NAME_FIELD + 4..][..4].try_into().unwrap());
        let stored =
            u32::from_le_bytes(buf[off + NAME_FIELD + 8..][..4].try_into().unwrap()) as usize;
        let pstart = off + hdr;
        if pstart + stored > buf.len() {
            return Err(Error::Container(format!(
                "image entry {idx} ({}): payload {stored} bytes at {pstart:#x} overruns the buffer",
                name.as_str()
            )));
        }
        out.push(ImageEntry {
            name,
            width: w,
            height: h,
            payload: buf[pstart..pstart + stored].to_vec(),
        });
        off = pstart + stored;
    }
    if off != buf.len() {
        return Err(Error::Container(format!(
            "{} trailing bytes after {count} image entries",
            buf.len() - off
        )));
    }
    Ok(out)
}

pub fn serialize_images(entries: &[ImageEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        out.extend_from_slice(e.name.field());
        out.extend_from_slice(&e.width.to_le_bytes());
        out.extend_from_slice(&e.height.to_le_bytes());
        out.extend_from_slice(&(e.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&e.payload);
    }
    out
}

pub fn parse_csvs(buf: &[u8]) -> Result<Vec<CsvEntry>, Error> {
    if buf.len() < 4 {
        return Err(Error::TooShort(buf.len()));
    }
    let count = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
    let mut off = 4usize;
    let mut out = Vec::with_capacity(count.min(4096));
    for idx in 0..count {
        if off + NAME_FIELD + 4 > buf.len() {
            return Err(Error::Container(format!(
                "csv entry {idx}: header at {off:#x} overruns the buffer"
            )));
        }
        let name = NameField::from_field(buf[off..off + NAME_FIELD].try_into().unwrap());
        let len =
            u32::from_le_bytes(buf[off + NAME_FIELD..][..4].try_into().unwrap()) as usize;
        let pstart = off + NAME_FIELD + 4;
        if pstart + len > buf.len() {
            return Err(Error::Container(format!(
                "csv entry {idx} ({}): payload {len} bytes at {pstart:#x} overruns the buffer",
                name.as_str()
            )));
        }
        out.push(CsvEntry {
            name,
            payload: buf[pstart..pstart + len].to_vec(),
        });
        off = pstart + len;
    }
    if off != buf.len() {
        return Err(Error::Container(format!(
            "{} trailing bytes after {count} csv entries",
            buf.len() - off
        )));
    }
    Ok(out)
}

pub fn serialize_csvs(entries: &[CsvEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        out.extend_from_slice(e.name.field());
        out.extend_from_slice(&(e.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&e.payload);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_container_exact_bytes_and_roundtrip() {
        let mut field = [0u8; NAME_FIELD];
        for (i, b) in "_d_kt80000_a.dds".encode_utf16().collect::<Vec<_>>().iter().enumerate() {
            field[i * 2..i * 2 + 2].copy_from_slice(&b.to_le_bytes());
        }
        let entries = vec![
            ImageEntry {
                name: NameField::from_field(&field),
                width: 512,
                height: 256,
                payload: vec![0xAB; 64],
            },
            ImageEntry {
                name: NameField::from_str("tiny").unwrap(),
                width: 0,
                height: 0,
                payload: vec![],
            },
        ];
        let ser = serialize_images(&entries);
        // count + 2 * (0x200 name + 12) + payloads
        assert_eq!(ser.len(), 4 + 2 * (NAME_FIELD + 12) + 64);
        assert_eq!(u32::from_le_bytes(ser[..4].try_into().unwrap()), 2);
        // name field starts right after the count, UTF-16LE + NUL + zero pad
        assert_eq!(&ser[4..4 + 8], b"_\0d\0_\0k\0");
        assert!(ser[4 + 34..4 + NAME_FIELD].iter().all(|&b| b == 0)); // padding
        // w/h/stored of entry 0
        let e0 = 4 + NAME_FIELD;
        assert_eq!(u32::from_le_bytes(ser[e0..e0 + 4].try_into().unwrap()), 512);
        assert_eq!(u32::from_le_bytes(ser[e0 + 4..e0 + 8].try_into().unwrap()), 256);
        assert_eq!(u32::from_le_bytes(ser[e0 + 8..e0 + 12].try_into().unwrap()), 64);

        let parsed = parse_images(&ser).unwrap();
        assert_eq!(parsed, entries);
        assert_eq!(parsed[0].name.as_str(), "_d_kt80000_a.dds");
        assert_eq!(serialize_images(&parsed), ser);
    }

    #[test]
    fn name_field_padding_garbage_survives_verbatim() {
        // original ARTDINK packs leave garbage in the padding after the NUL
        // (proven: leftover filename fragments in mttkmu0/sdp name fields)
        let mut field = [0u8; NAME_FIELD];
        for (i, b) in "_d_museum3_a.dds".encode_utf16().collect::<Vec<_>>().iter().enumerate() {
            field[i * 2..i * 2 + 2].copy_from_slice(&b.to_le_bytes());
        }
        // leftover fragment where a clean builder would zero-pad
        let frag = b"_b.dds".iter().map(|&b| u16::from(b)).collect::<Vec<_>>();
        for (i, u) in frag.iter().enumerate() {
            field[60 + i * 2..60 + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        let entry = ImageEntry {
            name: NameField::from_field(&field),
            width: 4,
            height: 4,
            payload: vec![1, 2, 3, 4],
        };
        let ser = serialize_images(&[entry.clone()]);
        assert_eq!(parse_images(&ser).unwrap(), vec![entry]);
        assert_eq!(parse_images(&ser).unwrap()[0].name.field(), &field);
        // decoded view stops at the NUL
        assert_eq!(parse_images(&ser).unwrap()[0].name.as_str(), "_d_museum3_a.dds");
    }

    #[test]
    fn image_container_rejects_trailing_and_truncated() {
        let entries = vec![ImageEntry {
            name: NameField::from_str("x").unwrap(),
            width: 1,
            height: 2,
            payload: vec![1, 2, 3],
        }];
        let ser = serialize_images(&entries);
        assert!(matches!(
            parse_images(&ser[..ser.len() - 1]),
            Err(Error::Container(_))
        ));
        let mut trailing = ser.clone();
        trailing.push(0);
        assert!(matches!(
            parse_images(&trailing),
            Err(Error::Container(_))
        ));
    }

    #[test]
    fn csv_container_roundtrip_and_shape() {
        let entries = vec![
            CsvEntry {
                name: NameField::from_str("train_comment.csv").unwrap(),
                payload: b"a,b,c\r\n".to_vec(),
            },
            CsvEntry {
                name: NameField::from_str("empty.csv").unwrap(),
                payload: vec![],
            },
        ];
        let ser = serialize_csvs(&entries);
        let payload_total: usize = entries.iter().map(|e| e.payload.len()).sum();
        assert_eq!(ser.len(), 4 + 2 * (NAME_FIELD + 4) + payload_total);
        let parsed = parse_csvs(&ser).unwrap();
        assert_eq!(parsed, entries);
        assert_eq!(serialize_csvs(&parsed), ser);
        let mut trunc = ser.clone();
        trunc.truncate(trunc.len() - 3);
        assert!(matches!(parse_csvs(&trunc), Err(Error::Container(_))));
    }

    #[test]
    fn name_too_long_and_nul_rejected_in_builders() {
        // 255 chars = 510 B of UTF-16 + the NUL = exactly 0x200
        let fits = "X".repeat(NAME_FIELD / 2 - 1);
        assert!(NameField::from_str(&fits).is_ok());
        assert_eq!(NameField::from_str(&fits).unwrap().as_str(), fits);
        // 256 chars = 512 B — no room for the NUL terminator
        for len in [NAME_FIELD / 2, NAME_FIELD / 2 + 1] {
            let too_long = "X".repeat(len);
            assert!(matches!(
                NameField::from_str(&too_long),
                Err(Error::NameTooLong { .. })
            ));
        }
        assert!(matches!(
            NameField::from_str("bad\0name"),
            Err(Error::Container(_))
        ));
    }

    #[test]
    fn dds_dims_helper() {
        let mut dds = vec![0u8; 20];
        dds[..4].copy_from_slice(b"DDS ");
        dds[12..16].copy_from_slice(&256u32.to_le_bytes()); // height
        dds[16..20].copy_from_slice(&512u32.to_le_bytes()); // width
        assert_eq!(dds_dims(&dds), (512, 256));
        assert_eq!(dds_dims(b"not a dds header at all"), (0, 0));
    }
}
