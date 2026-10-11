//! Savegame (`.a9d` / `.a9d5`) plaintext members — corpus
//! `notes/12-savegame-format.md`: `[24-byte "ARTDINK ATRAIN9\0" + u16 header]`
//! followed by a sequence of plain zlib members. Carving = scan for the
//! `78 9C` header, inflate exactly one member, advance by its true input
//! length, repeat.
//!
//! Members are inflated with flate2's low-level [`flate2::Decompress`] so the
//! consumed-input count is exact (streaming `Read` adapters may read ahead
//! past a member boundary, which would silently skip bytes here).

use flate2::{Decompress, FlushDecompress, Status};

/// Carve and inflate every zlib member found in `bytes` (best effort:
/// non-member stretches are skipped byte by byte).
pub fn carve_members(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut members = Vec::new();
    let mut pos = 0usize;
    while pos + 1 < bytes.len() {
        if bytes[pos] == 0x78 && bytes[pos + 1] == 0x9c {
            if let Some((data, used)) = inflate_member(&bytes[pos..]) {
                members.push(data);
                pos += used;
                continue;
            }
        }
        pos += 1;
    }
    members
}

/// Inflate exactly one zlib member from the front of `data`.
/// Returns `(plaintext, input bytes consumed)` or `None` if `data` does not
/// start a complete zlib stream.
pub fn inflate_member(data: &[u8]) -> Option<(Vec<u8>, usize)> {
    let mut d = Decompress::new(true); // zlib wrapper
    let mut out = Vec::new();
    let mut scratch = vec![0u8; 1 << 16];
    let mut in_pos = 0usize; // prefix of `data` consumed so far
    loop {
        let before_in = d.total_in();
        let before_out = d.total_out();
        let status = d
            .decompress(&data[in_pos..], &mut scratch, FlushDecompress::None)
            .ok()?;
        in_pos += (d.total_in() - before_in) as usize;
        let produced = (d.total_out() - before_out) as usize;
        out.extend_from_slice(&scratch[..produced]);
        match status {
            Status::StreamEnd => return Some((out, in_pos)),
            Status::Ok | Status::BufError => {
                if produced == 0 && in_pos >= data.len() {
                    return None; // needs input we don't have
                }
                if produced == 0 && d.total_in() == before_in {
                    return None; // no forward progress
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn single_and_multi_member_carve() {
        let m1 = zlib(b"member one content");
        let m2 = zlib(b"member two, somewhat longer, with repetition repetition repetition");
        let m3 = zlib(&vec![0u8; 10_000]);
        let mut blob = m1.clone();
        blob.extend_from_slice(b"\xDE\xAD junk between members \xBE\xEF");
        blob.extend_from_slice(&m2);
        blob.extend_from_slice(&m3);
        assert_eq!(carve_members(&blob), vec![
            b"member one content".to_vec(),
            b"member two, somewhat longer, with repetition repetition repetition".to_vec(),
            vec![0u8; 10_000],
        ]);
    }

    #[test]
    fn inflate_member_exact_consumption() {
        let member = zlib(b"payload");
        let mut data = member.clone();
        data.extend_from_slice(b"trailing bytes that are not part of the member");
        let (out, used) = inflate_member(&data).unwrap();
        assert_eq!(out, b"payload");
        assert_eq!(used, member.len());
    }

    #[test]
    fn garbage_returns_empty() {
        assert!(carve_members(b"no zlib members in here at all").is_empty());
        assert!(inflate_member(b"not a zlib stream........").is_none());
    }
}
