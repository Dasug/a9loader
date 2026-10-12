//! a9-formats — codecs for every A-Train 9 v5 data format, ported 1:1 from the
//! verified reference tools in the research corpus (`tools/`, A-Train 9
//! Decompilation folder) so that byte-parity is the acceptance criterion, not
//! a hope:
//!
//! - [`add`] / [`crypt`] — the pack cipher (`tools/a9crypt.py`, recovered from
//!   `FUN_140315710`): 16-byte MD5 header doubling as XOR key + ADD table.
//! - [`lzss`] — the LZSS codec (`tools/a9dec.c` decode, byte-exact against the
//!   game's `FUN_140314bf0`; `tools/a9enc.c` encode, the greedy hash-chain
//!   matcher that built the live repacked packs). Back-refs are copied
//!   **read-as-you-write** — the decode bug class that cost the previous
//!   project two weeks (corpus lesson 8.1) is impossible to reintroduce here
//!   without the round-trip tests catching it.
//! - [`pack`] — pack-file framing: family A (size-prefixed) vs family B
//!   (entry-table stream), `tools/repack.py` semantics.
//! - [`container`] — the two family-B container shapes: image packs
//!   (`rebuild_livery_packs.py`) and the 0x204 CSV variant (`build_csvp_ascii.py`).
//! - [`idat`] — the `i.dat` master index (`tools/i_parse.py`, verified
//!   byte-identical round-trip).
//! - [`savegame`] — plaintext zlib members of `.a9d`/`.a9d5` saves
//!   (corpus `notes/12-savegame-format.md`).
//!
//! `a9ml verify` is the machine that proves all of this against real packs.

pub mod add;
pub mod overlay;
pub mod container;
pub mod crypt;
pub mod idat;
pub mod lzss;
pub mod pack;
pub mod savegame;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("input too short ({0} bytes)")]
    TooShort(usize),
    #[error("MD5 header mismatch — not a valid pack for this cipher")]
    Md5Mismatch,
    #[error("family-A size prefix declares {declared} bytes but decoded {decoded}")]
    DeclaredSize { declared: usize, decoded: usize },
    #[error("container walk failed: {0}")]
    Container(String),
    #[error("i.dat: {0}")]
    Idat(String),
    #[error("entry name too long for its fixed field ({len} bytes of UTF-16)")]
    NameTooLong { len: usize },
}
