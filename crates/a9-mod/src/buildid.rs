//! Build identity — reads the game exe's `VS_VERSIONINFO` **from the file
//! bytes** (no `GetFileVersionInfoW`: that API is forwarded by our own proxy,
//! whose lazy real-DLL load must never happen inside `DllMain` — loader
//! lock). Pure bytes in, canonical build id out; identical code runs in the
//! loader (boot gate) and `a9ml doctor` (host).
//!
//! Canonical id format: `{major}.{minor:02}.{build}` with the patch component
//! inserted when nonzero — the international Steam exe's `FILEVERSION
//! 5,0,0,5634` becomes `"5.00.5634"`; JP retail's `5,0,0,4914` becomes
//! `"5.00.4914"`.

/// The running (or inspected) build's identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildId {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
    pub build: u16,
    /// canonical id, e.g. "5.00.5634"
    pub id: String,
}

impl BuildId {
    pub fn new(major: u16, minor: u16, patch: u16, build: u16) -> Self {
        let id = if patch == 0 {
            format!("{major}.{minor:02}.{build}")
        } else {
            format!("{major}.{minor:02}.{patch}.{build}")
        };
        Self { major, minor, patch, build, id }
    }

    /// The FILEVERSION tuple form, e.g. "5,0,0,5634".
    pub fn tuple(&self) -> String {
        format!("{},{},{},{}", self.major, self.minor, self.patch, self.build)
    }
}

/// Parse the exe image bytes and extract the FILEVERSION from the first
/// VERSIONINFO resource in `.rsrc`. `None` = no PE / no version info.
pub fn from_exe_bytes(exe: &[u8]) -> Option<BuildId> {
    let blob = versioninfo_blob(exe)?;
    let (major, minor, patch, build) = parse_versioninfo(&blob)?;
    Some(BuildId::new(major, minor, patch, build))
}

/// Walk PE headers → `.rsrc` → type 16 (VERSION) → first name → first lang →
/// the VERSIONINFO data blob.
fn versioninfo_blob(exe: &[u8]) -> Option<Vec<u8>> {
    if exe.len() < 0x40 || &exe[..2] != b"MZ" {
        return None;
    }
    let pe = u32::from_le_bytes(exe.get(0x3c..0x40)?.try_into().ok()?) as usize;
    if exe.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let coff = pe + 4;
    let n_sections = u16::from_le_bytes(exe.get(coff + 2..coff + 4)?.try_into().ok()?) as usize;
    let opt_size = u16::from_le_bytes(exe.get(coff + 16..coff + 18)?.try_into().ok()?) as usize;
    let opt = coff + 20;
    if exe.get(opt..opt + 2)? != &[0x0b, 0x02] {
        return None; // PE32+ only (the game is x64)
    }
    // data directory entry 2 (.rsrc) — at optional+112 in PE32+
    let dd = opt + 112 + 2 * 8;
    let rsrc_rva = u32::from_le_bytes(exe.get(dd..dd + 4)?.try_into().ok()?) as usize;
    if rsrc_rva == 0 {
        return None;
    }
    let sections = opt + opt_size;
    let rva_to_off = |rva: usize| -> Option<usize> {
        for s in 0..n_sections {
            let sh = sections + s * 40;
            let va = u32::from_le_bytes(exe.get(sh + 12..sh + 16)?.try_into().ok()?) as usize;
            let raw_size = u32::from_le_bytes(exe.get(sh + 16..sh + 20)?.try_into().ok()?) as usize;
            if va != 0 && rva >= va && rva < va + raw_size {
                let raw = u32::from_le_bytes(exe.get(sh + 20..sh + 24)?.try_into().ok()?) as usize;
                return Some(rva - va + raw);
            }
        }
        None
    };

    // resource directory helpers
    let dir_counts = |off: usize| -> Option<(usize, usize)> {
        let h = exe.get(off..off + 16)?;
        Some((
            u16::from_le_bytes(h[12..14].try_into().ok()?) as usize,
            u16::from_le_bytes(h[14..16].try_into().ok()?) as usize,
        ))
    };
    let dir_entry = |off: usize, i: usize| -> Option<(u32, u32)> {
        let e = exe.get(off + 16 + i * 8..off + 16 + i * 8 + 8)?;
        Some((
            u32::from_le_bytes(e[0..4].try_into().ok()?),
            u32::from_le_bytes(e[4..8].try_into().ok()?),
        ))
    };
    let subdir = |rsrc_rva: usize, off_to_data: u32| -> Option<usize> {
        if off_to_data & 0x8000_0000 == 0 {
            return None; // not a directory
        }
        rva_to_off(rsrc_rva + (off_to_data & 0x7fff_ffff) as usize)
    };

    // level 1: find resource type 16 (VERSION)
    let root = rva_to_off(rsrc_rva)?;
    let (named, ids) = dir_counts(root)?;
    let mut version_dir = None;
    for i in 0..named + ids {
        let (name, off) = dir_entry(root, i)?;
        if name & 0x8000_0000 == 0 && name == 16 {
            version_dir = Some(subdir(rsrc_rva, off));
            break;
        }
    }
    // level 2: first entry (any) -> name directory
    let type_dir = version_dir??;
    let (_, off2) = dir_entry(type_dir, 0)?;
    let name_dir = subdir(rsrc_rva, off2)?;
    // level 3: first entry -> IMAGE_RESOURCE_DATA_ENTRY (offset relative to .rsrc)
    let (_, off3) = dir_entry(name_dir, 0)?;
    if off3 & 0x8000_0000 != 0 {
        return None; // another directory level — unexpected for VERSIONINFO
    }
    let data_entry = rva_to_off(rsrc_rva + off3 as usize)?;
    let data_rva = u32::from_le_bytes(exe.get(data_entry..data_entry + 4)?.try_into().ok()?) as usize;
    let size = u32::from_le_bytes(exe.get(data_entry + 4..data_entry + 8)?.try_into().ok()?) as usize;
    let data_off = rva_to_off(data_rva)?;
    Some(exe.get(data_off..data_off + size)?.to_vec())
}

/// Extract (major, minor, patch, build) from a VS_VERSIONINFO blob by
/// locating the `VS_FIXEDFILEINFO` signature `0xFEEF04BD`.
fn parse_versioninfo(blob: &[u8]) -> Option<(u16, u16, u16, u16)> {
    let sig = [0xbd, 0x04, 0xef, 0xfe];
    let scan = &blob[..blob.len().min(96)];
    let pos = scan.windows(4).position(|w| w == sig)?;
    let g = |o: usize| -> Option<u32> {
        Some(u32::from_le_bytes(blob.get(pos + o..pos + o + 4)?.try_into().ok()?))
    };
    let ms = g(8)?;
    let ls = g(12)?;
    Some((
        (ms >> 16) as u16,
        (ms & 0xffff) as u16,
        (ls >> 16) as u16,
        (ls & 0xffff) as u16,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal PE32+ image with one `.rsrc` section carrying a
    /// VERSIONINFO resource with the given FILEVERSION.
    fn synth_exe(fileversion: (u16, u16, u16, u16)) -> Vec<u8> {
        let rsrc_rva: usize = 0x1000;
        let rsrc_off: usize = 0x400;

        let (major, minor, patch, build) = fileversion;
        let ms = ((major as u32) << 16) | minor as u32;
        let ls = ((patch as u32) << 16) | build as u32;
        let mut blob = Vec::new();
        blob.extend_from_slice(&200u16.to_le_bytes()); // wLength (loose)
        blob.extend_from_slice(&0u16.to_le_bytes()); // wValueLength
        blob.extend_from_slice(&0u16.to_le_bytes()); // wType
        for u in "VS_VERSION_INFO\0".encode_utf16() {
            blob.extend_from_slice(&u.to_le_bytes());
        }
        while blob.len() % 4 != 0 {
            blob.push(0);
        }
        blob.extend_from_slice(&0xFEEF_04BDu32.to_le_bytes()); // dwSignature
        blob.extend_from_slice(&0x0001_0000u32.to_le_bytes()); // dwStrucVersion
        blob.extend_from_slice(&ms.to_le_bytes()); // dwFileVersionMS
        blob.extend_from_slice(&ls.to_le_bytes()); // dwFileVersionLS
        blob.extend_from_slice(&[0u8; 36]); // rest of VS_FIXEDFILEINFO

        fn push_dir(rsrc: &mut Vec<u8>, entries: &[[u8; 8]]) {
            rsrc.extend_from_slice(&[0u8; 12]); // Characteristics, TimeDateStamp, Version
            rsrc.extend_from_slice(&0u16.to_le_bytes()); // NumberOfNamedEntries
            rsrc.extend_from_slice(&(entries.len() as u16).to_le_bytes()); // NumberOfIdEntries
            for e in entries {
                rsrc.extend_from_slice(e);
            }
        }
        fn entry(name: u32, off: u32) -> [u8; 8] {
            let mut e = [0u8; 8];
            e[0..4].copy_from_slice(&name.to_le_bytes());
            e[4..8].copy_from_slice(&off.to_le_bytes());
            e
        }
        // offsets relative to the .rsrc start
        let name_dir = 0x30u32;
        let lang_dir = 0x50u32;
        let data_entry = 0x70u32;
        let data_blob = 0x80u32;
        let mut rsrc = Vec::new();
        push_dir(&mut rsrc, &[entry(16, 0x8000_0000 | name_dir)]); // type 16 -> dir
        while rsrc.len() < name_dir as usize {
            rsrc.push(0);
        }
        push_dir(&mut rsrc, &[entry(1, 0x8000_0000 | lang_dir)]); // name/id 1 -> dir
        while rsrc.len() < lang_dir as usize {
            rsrc.push(0);
        }
        push_dir(&mut rsrc, &[entry(0x409, data_entry)]); // lang -> data entry
        while rsrc.len() < data_entry as usize {
            rsrc.push(0);
        }
        // IMAGE_RESOURCE_DATA_ENTRY
        rsrc.extend_from_slice(&((rsrc_rva + data_blob as usize) as u32).to_le_bytes());
        rsrc.extend_from_slice(&(blob.len() as u32).to_le_bytes());
        rsrc.extend_from_slice(&0u32.to_le_bytes()); // CodePage
        rsrc.extend_from_slice(&0u32.to_le_bytes()); // Reserved
        while rsrc.len() < data_blob as usize {
            rsrc.push(0);
        }
        rsrc.extend_from_slice(&blob);

        // PE headers + one section mapping .rsrc
        let opt_size = 240usize; // PE32+ standard size
        let headers = 0x18 + 20 + opt_size + 40;
        let mut img = vec![0u8; headers.max(rsrc_off + rsrc.len())];
        img[0] = b'M';
        img[1] = b'Z';
        img[0x3c..0x40].copy_from_slice(&0x18u32.to_le_bytes());
        let pe = 0x18usize;
        img[pe..pe + 4].copy_from_slice(b"PE\0\0");
        let coff = pe + 4;
        img[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes()); // 1 section
        img[coff + 16..coff + 18].copy_from_slice(&(opt_size as u16).to_le_bytes());
        let opt = coff + 20;
        img[opt..opt + 2].copy_from_slice(&[0x0b, 0x02]); // PE32+ magic
        let dd = opt + 112 + 2 * 8;
        img[dd..dd + 4].copy_from_slice(&(rsrc_rva as u32).to_le_bytes());
        img[dd + 4..dd + 8].copy_from_slice(&(rsrc.len() as u32).to_le_bytes());
        let sh = opt + opt_size;
        img[sh..sh + 8].copy_from_slice(b".rsrc\0\0\0");
        img[sh + 8..sh + 12].copy_from_slice(&(rsrc.len() as u32).to_le_bytes()); // VirtualSize
        img[sh + 12..sh + 16].copy_from_slice(&(rsrc_rva as u32).to_le_bytes()); // VirtualAddress
        img[sh + 16..sh + 20].copy_from_slice(&(rsrc.len() as u32).to_le_bytes()); // SizeOfRawData
        img[sh + 20..sh + 24].copy_from_slice(&(rsrc_off as u32).to_le_bytes()); // PointerToRawData
        img[rsrc_off..rsrc_off + rsrc.len()].copy_from_slice(&rsrc);
        img
    }

    #[test]
    fn synthetic_pe_parses() {
        let exe = synth_exe((5, 0, 0, 5634));
        let id = from_exe_bytes(&exe).unwrap();
        assert_eq!(id.id, "5.00.5634");
        assert_eq!(id.tuple(), "5,0,0,5634");
        assert_eq!(from_exe_bytes(&synth_exe((5, 0, 0, 4914))).unwrap().id, "5.00.4914");
        assert_eq!(from_exe_bytes(&synth_exe((2, 3, 1, 9))).unwrap().id, "2.03.1.9");
    }

    #[test]
    fn rejects_non_pe() {
        assert!(from_exe_bytes(b"").is_none());
        assert!(from_exe_bytes(b"not a PE file at all....").is_none());
        let exe = synth_exe((1, 0, 0, 1));
        let mut broken = exe;
        broken[0x3c] = 0x00; // trash e_lfanew
        assert!(from_exe_bytes(&broken).is_none());
    }

    #[test]
    fn real_exes_when_available() {
        // env-gated: run on this machine against the live installs
        for (env, expected) in [("A9_STEAM_EXE", "5.00.5634"), ("A9_JP_EXE", "5.00.4914")] {
            if let Ok(path) = std::env::var(env) {
                let bytes = std::fs::read(&path).unwrap();
                assert_eq!(from_exe_bytes(&bytes).unwrap().id, expected, "{path}");
            }
        }
    }
}
