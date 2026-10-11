//! The LZSS codec — exact ports of corpus `tools/a9dec.c` (decode, verified
//! byte-exact against the game's `FUN_140314bf0`) and `tools/a9enc.c`
//! (encode: the greedy hash-chain matcher that built the live repacked packs,
//! documented as semantically identical to `tools/lz_pack.py compress()`).
//!
//! Format: 4096-byte ring zeroed on start with the write cursor at `0xfee`,
//! LSB-first flag bytes, bit=1 → literal, bit=0 → back-ref
//! (`pos = b3 | ((b4 & 0xf0) << 4)`, `len = (b4 & 0xf) + 3`).
//! Back-refs are copied **read-as-you-write** — an overlapping ref
//! self-extends, and a snapshot-copy decoder silently corrupts everything
//! after the first overlap (corpus lesson 8.1).
//!
//! Byte-parity of the encoder with `a9enc` is asserted by `a9ml verify
//! --oracle` on real packs; decode round-trips are unit-tested here.

pub const WIN: usize = 4096;
const MASK: usize = WIN - 1;
pub const MIN_LEN: usize = 3;
pub const MAX_LEN: usize = 18;
pub const RING_START: usize = 0xfee;
const HASH_BITS: u32 = 22;
const HASH_SIZE: usize = 1 << HASH_BITS;
const HASH_MASK: usize = HASH_SIZE - 1;
const MAX_CHAIN_STEPS: usize = 4096;

/// Decode `data[skip..]` until the input is exhausted — mirrors
/// `lz_pack.decompress` / `a9dec` (which never stop at a declared size).
pub fn decode(data: &[u8], skip: usize) -> Vec<u8> {
    let mut ring = [0u8; WIN];
    let mut r = RING_START;
    let mut out = Vec::new();
    let mut flags = 0u8;
    let mut nbits = 0;
    let mut pos = skip;
    while pos < data.len() {
        if nbits == 0 {
            flags = data[pos];
            pos += 1;
            nbits = 8;
        }
        let bit = flags & 1;
        flags >>= 1;
        nbits -= 1;
        if bit != 0 {
            if pos >= data.len() {
                break;
            }
            let c = data[pos];
            pos += 1;
            out.push(c);
            ring[r] = c;
            r = (r + 1) & MASK;
        } else {
            if pos + 1 >= data.len() {
                break;
            }
            let b3 = data[pos];
            let b4 = data[pos + 1];
            pos += 2;
            let p = (((b4 & 0xf0) as usize) << 4) | b3 as usize;
            let len = (b4 & 0x0f) as usize + 3;
            for i in 0..len {
                // read-as-you-write: the source slot may be one we just wrote
                let c = ring[(p + i) & MASK];
                out.push(c);
                ring[r] = c;
                r = (r + 1) & MASK;
            }
        }
    }
    out
}

/// Encode `raw` — an exact port of `tools/a9enc.c` (greedy longest-match with
/// a 22-bit hash-3 chain, chain cap 4096 steps, window distance ≤ 4096,
/// match insert-position rules included). The output must stay byte-identical
/// to `a9enc` — that parity is `a9ml verify --oracle`'s job.
pub fn encode(raw: &[u8]) -> Vec<u8> {
    let n = raw.len();
    // a9enc stores positions as int32; guard the same limit explicitly
    debug_assert!(n <= i32::MAX as usize, "input larger than the C reference handles");
    let mut out: Vec<u8> = Vec::with_capacity(n + n / 8 + 64);
    let mut ring = [0u8; WIN];
    let mut r = RING_START;
    let mut head = vec![-1i32; HASH_SIZE];
    let mut prev = vec![-1i32; n + 8];
    let h3 = |i: usize| -> usize {
        (((raw[i] as u32) << 16) ^ ((raw[i + 1] as u32) << 8) ^ raw[i + 2] as u32) as usize
            & HASH_MASK
    };
    let mut i = 0usize;
    let mut flagpos: Option<usize> = None;
    let mut flags = 0u8;
    let mut bitidx = 0u32;
    while i < n {
        if flagpos.is_none() {
            flagpos = Some(out.len());
            out.push(0);
            flags = 0;
            bitidx = 0;
        }
        let mut bestlen = 0usize;
        let mut bestpos = 0usize;
        if i + 2 < n {
            let h = h3(i);
            let mut cand = head[h];
            let mut steps = 0usize;
            while cand >= 0 && i - cand as usize <= WIN && steps < MAX_CHAIN_STEPS {
                let mut l = 0usize;
                let mut j = cand as usize;
                let mut k = i;
                while l < MAX_LEN && k < n && raw[j] == raw[k] {
                    l += 1;
                    j += 1;
                    k += 1;
                }
                if l > bestlen {
                    bestlen = l;
                    bestpos = cand as usize;
                    if l >= MAX_LEN {
                        break;
                    }
                }
                cand = prev[cand as usize];
                steps += 1;
            }
        }
        if bestlen >= MIN_LEN {
            // flag bit 0 (match): the bit stays 0
            bitidx += 1;
            let pos_ref = (RING_START + bestpos) & MASK;
            out.push((pos_ref & 0xff) as u8);
            out.push(((((pos_ref >> 8) & 0xf) << 4) | (bestlen - MIN_LEN)) as u8);
            for k in 0..bestlen {
                let c = raw[i + k];
                ring[r] = c;
                r = (r + 1) & MASK;
                let ip = i + k;
                if ip + 2 < n {
                    let hh = h3(ip);
                    prev[ip] = head[hh];
                    head[hh] = ip as i32;
                }
            }
            i += bestlen;
        } else {
            flags |= 1u8 << bitidx;
            bitidx += 1;
            let c = raw[i];
            out.push(c);
            ring[r] = c;
            r = (r + 1) & MASK;
            if i + 2 < n {
                let hh = h3(i);
                prev[i] = head[hh];
                head[hh] = i as i32;
            }
            i += 1;
        }
        out[flagpos.unwrap()] = flags;
        if bitidx == 8 {
            flagpos = None;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(x: &[u8]) {
        assert_eq!(decode(&encode(x), 0), x, "round-trip failed for len {}", x.len());
    }

    #[test]
    fn known_streams() {
        // flag bit0 = 1 → literal 'A'
        assert_eq!(decode(&[0x01, b'A'], 0), b"A");
        // flag bit0 = 0 → back-ref p=0 len=3 into the zero-initialised ring
        assert_eq!(decode(&[0x00, 0x00, 0x00], 0), [0u8, 0, 0]);
        // skip (the family-A size prefix): first byte ignored
        assert_eq!(decode(&[0xff, 0x01, b'A'], 1), b"A");
    }

    #[test]
    fn truncated_stream_breaks_cleanly() {
        // flag byte promises a literal but stream ends: decode stops, no panic
        assert_eq!(decode(&[0x01], 0), b"");
        // back-ref needs two bytes, only one present
        assert_eq!(decode(&[0x00, 0x01], 0), b"");
    }

    #[test]
    fn encode_decode_roundtrip() {
        rt(b"");
        rt(b"a");
        rt(b"ab");
        rt(b"aaaaaaaaaaaaaaaaaaaaaaaaa"); // > MAX_LEN run
        rt(b"abcabcabcabcabcabcabc"); // overlapping self-extension
        rt(b"The quick brown fox jumps over the lazy dog. The quick brown fox jumps over the lazy dog again and again and again.");
    }

    #[test]
    fn encode_decode_random_and_mixed() {
        let mut x = 0x1234_5678_9abc_def0u64;
        let mut buf = vec![0u8; 100_000];
        for b in buf.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = (x >> 56) as u8;
        }
        rt(&buf);
        // pseudo-DDS mix: header + big zero runs + random blocks
        let mut mix = vec![0u8; 40_000];
        mix[..4].copy_from_slice(b"DDS ");
        for (i, b) in mix.iter_mut().enumerate().skip(20_000) {
            *b = (i * 7 + (i >> 5)) as u8;
        }
        rt(&mix);
    }

    #[test]
    fn window_wrap_and_reuse() {
        // > 4096 bytes so the ring wraps and old slots get overwritten
        let mut buf = vec![0u8; 9000];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        rt(&buf);
    }
}
