//! Pack-file cipher — port of corpus `tools/a9crypt.py` (scheme recovered from
//! `FUN_140315710`; the header doubles as the integrity check per
//! `FUN_140315790`):
//!
//! ```text
//! file  = [16-byte header = MD5(plain body)][encrypted body]
//! enc:  body[i] = ((plain[i] ^ hdr[i & 0xf]) - ADD[i & 0xff]) & 0xff  // XOR, then subtract
//! dec:  body[i] = ((body[i]  + ADD[i & 0xff]) & 0xff) ^ hdr[i & 0xf]
//! ```
//!
//! Re-encrypting is safe exactly while the header stays `MD5(plain)` — the
//! game's own integrity check.

use crate::add::ADD;
use md5::{Digest, Md5};

pub const HEADER_LEN: usize = 16;

/// MD5 of the plaintext body — the 16-byte pack header value.
pub fn md5(plain: &[u8]) -> [u8; 16] {
    let mut h = Md5::new();
    h.update(plain);
    h.finalize().into()
}

/// Decrypt the body (everything after the 16-byte header). Does NOT verify
/// MD5 — pair with [`header_matches`]. Byte-identical to `a9crypt.dec_body`.
pub fn decrypt_body(file: &[u8]) -> Result<Vec<u8>, crate::Error> {
    if file.len() < HEADER_LEN {
        return Err(crate::Error::TooShort(file.len()));
    }
    let hdr = &file[..HEADER_LEN];
    Ok(file[HEADER_LEN..]
        .iter()
        .enumerate()
        .map(|(i, &b)| b.wrapping_add(ADD[i & 0xff]) ^ hdr[i & 0xf])
        .collect())
}

/// True iff the file's header equals `MD5(plain)` — the game's integrity check.
pub fn header_matches(file: &[u8], plain: &[u8]) -> bool {
    file.len() >= HEADER_LEN && file[..HEADER_LEN] == md5(plain)
}

/// Encrypt into a full pack file: `header = MD5(plain)` + encrypted body.
/// Byte-identical to `a9crypt.enc_body(plain, md5(plain))`.
pub fn encrypt_file(plain: &[u8]) -> Vec<u8> {
    let hdr = md5(plain);
    let mut out = Vec::with_capacity(HEADER_LEN + plain.len());
    out.extend_from_slice(&hdr);
    out.extend(
        plain
            .iter()
            .enumerate()
            .map(|(i, &b)| (b ^ hdr[i & 0xf]).wrapping_sub(ADD[i & 0xff])),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xorshift(bytes: &mut [u8]) {
        let mut x = 0x1234_5678_9abc_def0u64;
        for b in bytes {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = (x >> 56) as u8;
        }
    }

    #[test]
    fn add_table_shape() {
        assert_eq!(ADD.len(), 256);
        // endpoints from the cross-checked generation (exe offset 0x697310)
        assert_eq!(&ADD[..4], &[0x3b, 0x16, 0x6f, 0xd8]);
        assert_eq!(&ADD[252..], &[0x6b, 0x2f, 0xe9, 0xdb]);
    }

    #[test]
    fn roundtrip_random_and_empty() {
        for plain in [&b""[..], b"hello world", &[0u8; 64][..]] {
            let file = encrypt_file(plain);
            assert_eq!(file.len(), HEADER_LEN + plain.len());
            assert!(header_matches(&file, plain));
            assert_eq!(decrypt_body(&file).unwrap(), plain);
        }
        let mut buf = vec![0u8; 100_000];
        xorshift(&mut buf);
        let file = encrypt_file(&buf);
        assert!(header_matches(&file, &buf));
        assert_eq!(decrypt_body(&file).unwrap(), buf);
    }

    #[test]
    fn decrypt_rejects_short_input() {
        assert!(decrypt_body(&[0u8; 15]).is_err());
        assert_eq!(decrypt_body(&[7u8; 16]).unwrap(), vec![7u8; 0]);
    }
}
