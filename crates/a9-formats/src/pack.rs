//! Pack-file framing — the two families:
//!
//! - **Family A** (size-prefixed): decrypted body = `[u32 declared out size]
//!   [LZSS stream]` — `tp`, `mcp`, `vp`, `ep`, `cp`, `i.dat`.
//! - **Family B** (entry-table): decrypted body = `[LZSS stream]` decoding
//!   straight into the entry table — `me0..3`, `mt*`, `csvp`, `sdp`, `csdp`.
//!
//! Family-A size checking uses the tolerance validated by the reference decoder's
//! auto mode (`max(declared/1000, 4096)` slack). Encode is `repack.py`
//! semantics: family A re-prefixes `[u32 len][LZSS]`, family B is bare LZSS;
//! both wrap with the MD5-header cipher.

use crate::{crypt, lzss};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Family {
    /// `[u32 size][LZSS]` body — tp/mcp/vp/ep/cp/i.dat
    A,
    /// bare `[LZSS]` body — me*/mt*/csvp/sdp/csdp
    B,
}

/// Which decoded shape a pack carries (drives container validation).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Container {
    /// family-B image pack: `[count][0x200 name][w][h][stored][payload]…`
    Images,
    /// family-B CSV pack (csvp/sdp/csdp): `[count][0x200 name][u32 len][data]…`
    Csvs,
    /// family-A `i.dat`: the master index rows
    Idat,
    /// no structural validation (family-A model/config streams)
    Opaque,
}

/// Classify a pack by filename (basename, case-insensitive) using the game's
/// own load-table behavior). `None` = not a pack we
/// decode (kit markers, stubs, unknown names).
pub fn classify(name: &str) -> Option<(Family, Container)> {
    let n = name.to_ascii_lowercase();
    let n = n.strip_suffix(".bin").unwrap_or(&n);
    match n {
        "i.dat" => Some((Family::A, Container::Idat)),
        "tp" | "mcp" | "vp" | "ep" | "cp" => Some((Family::A, Container::Opaque)),
        "csvp" | "sdp" | "csdp" => Some((Family::B, Container::Csvs)),
        _ => {
            let (pre, rest) = n.split_at(n.len().min(2));
            match pre {
                "me" if rest.bytes().all(|b| b.is_ascii_digit()) && !rest.is_empty() => {
                    Some((Family::B, Container::Images))
                }
                "mt" if !rest.is_empty() => Some((Family::B, Container::Images)),
                _ => None,
            }
        }
    }
}

/// The key [`classify`] matches on (lowercased basename, `.bin` stripped).
pub fn classify_key(name: &str) -> String {
    let n = name.to_ascii_lowercase();
    n.strip_suffix(".bin").unwrap_or(&n).to_string()
}

/// Decrypt + integrity-check + LZSS-decode a pack file.
/// Family A also validates the declared output size (reference-decoder tolerance).
pub fn decode_pack(file: &[u8], family: Family) -> Result<Vec<u8>, crate::Error> {
    let plain = crypt::decrypt_body(file)?;
    if !crypt::header_matches(file, &plain) {
        return Err(crate::Error::Md5Mismatch);
    }
    match family {
        Family::A => {
            if plain.len() < 4 {
                return Err(crate::Error::TooShort(plain.len()));
            }
            let declared = u32::from_le_bytes(plain[..4].try_into().unwrap()) as usize;
            let decoded = lzss::decode(&plain, 4);
            let tol = (declared / 1000).max(4096);
            if decoded.len() < declared || decoded.len() - declared > tol {
                return Err(crate::Error::DeclaredSize { declared, decoded: decoded.len() });
            }
            Ok(decoded)
        }
        Family::B => Ok(lzss::decode(&plain, 0)),
    }
}

/// The **declared output size** of a family-A pack FILE: the u32 that sits
/// immediately before the LZSS stream in the decrypted body.
///
/// Cheap by construction: the cipher is position-indexed, so the first four
/// plaintext bytes need only `ADD[0..4]` and `hdr[0..4]` — this decrypts
/// exactly those four bytes, never the LZSS stream and never the whole body.
/// No MD5 check either (that would require the full plaintext); the caller
/// cross-checks the value where it matters — the tailgate appends at
/// `dest + declared` and asserts the runtime decoded length equals it.
///
/// For the vanilla packs the declared size equals the decoded length (a9ml
/// `inspect` shows vp/tp/i.dat equal), which is what makes `|V|` derivable at
/// boot without decompressing.
pub fn family_a_declared_size(file: &[u8]) -> Result<usize, crate::Error> {
    use crate::add::ADD;
    if file.len() < crypt::HEADER_LEN + 4 {
        return Err(crate::Error::TooShort(file.len()));
    }
    let hdr = &file[..crypt::HEADER_LEN];
    let mut d = [0u8; 4];
    for (i, b) in d.iter_mut().enumerate() {
        // dec: plain[i] = ((enc[i] + ADD[i & 0xff]) & 0xff) ^ hdr[i & 0xf]
        *b = (file[crypt::HEADER_LEN + i].wrapping_add(ADD[i])) ^ hdr[i];
    }
    Ok(u32::from_le_bytes(d) as usize)
}

/// Re-materialize a family-A pack file: `[u32 len][LZSS(decoded)]` + cipher.
/// Byte-identical to `repack.py write_file` given the same LZSS encoder.
pub fn encode_pack_a(decoded: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(4 + decoded.len() / 2);
    body.extend_from_slice(&(decoded.len() as u32).to_le_bytes());
    body.extend_from_slice(&lzss::encode(decoded));
    crypt::encrypt_file(&body)
}

/// Re-materialize a family-B pack file: bare `[LZSS(decoded)]` + cipher.
pub fn encode_pack_b(decoded: &[u8]) -> Vec<u8> {
    crypt::encrypt_file(&lzss::encode(decoded))
}

/// Convenience: encode by family.
pub fn encode_pack(decoded: &[u8], family: Family) -> Vec<u8> {
    match family {
        Family::A => encode_pack_a(decoded),
        Family::B => encode_pack_b(decoded),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_table() {
        assert_eq!(classify("i.dat"), Some((Family::A, Container::Idat)));
        assert_eq!(classify("TP.BIN"), Some((Family::A, Container::Opaque)));
        assert_eq!(classify("vp.bin"), Some((Family::A, Container::Opaque)));
        assert_eq!(classify("me3.bin"), Some((Family::B, Container::Images)));
        assert_eq!(classify("ME0.bin"), Some((Family::B, Container::Images)));
        assert_eq!(classify("mtentr0.bin"), Some((Family::B, Container::Images)));
        assert_eq!(classify("mtdxtr0.bin"), Some((Family::B, Container::Images)));
        assert_eq!(classify("csvp.bin"), Some((Family::B, Container::Csvs)));
        assert_eq!(classify("sdp.bin"), Some((Family::B, Container::Csvs)));
        // not packs:
        assert_eq!(classify("ak.bin"), None);
        assert_eq!(classify("tk.bin"), None);
        assert_eq!(classify("structure_kit.bin"), None);
        assert_eq!(classify("mep3.bin"), None); // me + non-digit suffix
        assert_eq!(classify_key("Me3.BIN"), "me3");
        assert_eq!(classify_key("i.dat"), "i.dat");
    }

    #[test]
    fn family_a_roundtrip() {
        let raw = b"the quick brown fox jumps over the lazy dog, repeatedly, repeatedly, repeatedly";
        let file = encode_pack_a(raw);
        assert_eq!(decode_pack(&file, Family::A).unwrap(), raw);
        // md5 header valid on our own output
        let plain = crypt::decrypt_body(&file).unwrap();
        assert!(crypt::header_matches(&file, &plain));
    }

    #[test]
    fn family_b_roundtrip() {
        let raw = b"entry table bytes, entry table bytes, entry table bytes, again";
        let file = encode_pack_b(raw);
        assert_eq!(decode_pack(&file, Family::B).unwrap(), raw);
    }

    #[test]
    fn family_a_declared_mismatch_detected() {
        let raw = b"some content of a plausible size here, yes yes yes";
        let file = encode_pack_a(raw);
        // corrupt the declared size inside the plaintext, keep the md5 valid
        let mut plain = crypt::decrypt_body(&file).unwrap();
        plain[..4].copy_from_slice(&999999u32.to_le_bytes());
        let tampered = crypt::encrypt_file(&plain);
        assert!(matches!(
            decode_pack(&tampered, Family::A),
            Err(crate::Error::DeclaredSize { .. })
        ));
    }

    #[test]
    fn md5_tamper_detected() {
        let raw = b"content that is long enough to look like a body, sure, why not";
        let mut file = encode_pack_b(raw);
        let last = file.len() - 1;
        file[last] ^= 0xff;
        assert!(matches!(decode_pack(&file, Family::B), Err(crate::Error::Md5Mismatch)));
    }

    #[test]
    fn family_a_declared_size_reads_without_decompressing() {
        // a synthetic pack: the reader must return what encode_pack_a declared
        let raw: Vec<u8> = (0..4096u32).map(|i| (i * 31) as u8).collect();
        let file = encode_pack_a(&raw);
        assert_eq!(family_a_declared_size(&file).unwrap(), raw.len());
        // empty body: declared 0
        let empty = encode_pack_a(&[]);
        assert_eq!(family_a_declared_size(&empty).unwrap(), 0);
        // too short to carry the size word
        assert!(family_a_declared_size(&[0u8; 15]).is_err());
        assert!(family_a_declared_size(&[0u8; 19]).is_err());
        // a declared size that disagrees with the stream still READS (the
        // reader is a plain header peek — no LZSS, no MD5)
        let mut plain = crypt::decrypt_body(&file).unwrap();
        plain[..4].copy_from_slice(&123_456u32.to_le_bytes());
        let tampered = crypt::encrypt_file(&plain);
        assert_eq!(family_a_declared_size(&tampered).unwrap(), 123_456);
    }

    #[test]
    fn family_a_declared_size_real_packs_when_available() {
        let Ok(dir) = std::env::var("A9_DATA_DIR") else { return };
        for name in ["vp.bin", "tp.bin", "i.dat"] {
            let p = std::path::Path::new(&dir).join(name);
            let Ok(file) = std::fs::read(&p) else { continue };
            let declared = family_a_declared_size(&file).unwrap();
            // the declared size is a cheap peek; prove it equals the real
            // decoded length (the property the tailgate's |V| relies on)
            let decoded = decode_pack(&file, Family::A).unwrap();
            assert_eq!(
                declared,
                decoded.len(),
                "{name}: declared != decoded on the live install"
            );
        }
    }
}
