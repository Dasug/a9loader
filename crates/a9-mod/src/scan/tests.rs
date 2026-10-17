    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("a9mod-scan-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn scans_parses_and_sorts() {
        let dir = tmpdir("ok");
        std::fs::create_dir_all(dir.join("b-mod")).unwrap();
        std::fs::create_dir_all(dir.join("a-mod")).unwrap();
        std::fs::write(
            dir.join("a-mod/mod.toml"),
            "id=\"a\"\nname=\"A\"\nversion=\"1.0.0\"\nauthor=\"t\"\npriority=10\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("b-mod/mod.toml"),
            "id=\"b\"\nname=\"B\"\nversion=\"1.0.0\"\nauthor=\"t\"\n",
        )
        .unwrap();
        let scanned = scan(&dir);
        assert_eq!(scanned.len(), 2);
        assert!(scanned[0].dir.ends_with("a-mod"));
        assert_eq!(scanned[0].manifest.as_ref().unwrap().id, "a");
        assert_eq!(scanned[1].manifest.as_ref().unwrap().id, "b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_mods_dir_is_empty_silent() {
        assert!(scan(Path::new("/nonexistent-mods")).is_empty());
    }

    #[test]
    fn primes_row_keys_for_row_mode_csv_sections() {
        let dir = tmpdir("csvkeys");
        std::fs::create_dir_all(dir.join("m")).unwrap();
        std::fs::write(
            dir.join("m/mod.toml"),
            "id=\"m\"\nname=\"M\"\nversion=\"1.0.0\"\nauthor=\"t\"\n\
             [[csv]]\nname=\"train_comment.csv\"\nfile=\"p.csv\"\nmode=\"rows\"\n\
             [[csv]]\nname=\"train_form.csv\"\nfile=\"whole.csv\"\n",
        )
        .unwrap();
        // a UTF-16 patch file (BOM, CRLF, quoted)
        let mut patch = vec![0xff, 0xfe];
        for u in "\"FORM_1\",\"name\"\r\n\"FORM_2\",\"name2\"\r\n".encode_utf16() {
            patch.extend_from_slice(&u.to_le_bytes());
        }
        std::fs::write(dir.join("m/p.csv"), &patch).unwrap();
        std::fs::write(dir.join("whole.csv"), b"not parsed").unwrap();
        let scanned = scan(&dir);
        let m = scanned[0].manifest.as_ref().unwrap();
        assert_eq!(m.csv[0].row_keys, vec!["FORM_1".to_string(), "FORM_2".to_string()]);
        // replace mode is never primed
        assert!(m.csv[1].row_keys.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn primes_row_keys_from_a_utf8_patch_file() {
        // a plain UTF-8 patch file (what an editor writes) is accepted too
        let dir = tmpdir("csvkeys-utf8");
        std::fs::create_dir_all(dir.join("m")).unwrap();
        std::fs::write(
            dir.join("m/mod.toml"),
            "id=\"m\"\nname=\"M\"\nversion=\"1.0.0\"\nauthor=\"t\"\n\
             [[csv]]\nname=\"train_comment.csv\"\nfile=\"p.csv\"\nmode=\"rows\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("m/p.csv"), b"\"FORM_7\",\"x\"\r\n\"FORM_8\",\"y\"\r\n").unwrap();
        let scanned = scan(&dir);
        let m = scanned[0].manifest.as_ref().unwrap();
        assert_eq!(m.csv[0].row_keys, vec!["FORM_7".to_string(), "FORM_8".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_patch_file_leaves_keys_empty() {
        let dir = tmpdir("csvkeys-missing");
        std::fs::create_dir_all(dir.join("m")).unwrap();
        std::fs::write(
            dir.join("m/mod.toml"),
            "id=\"m\"\nname=\"M\"\nversion=\"1.0.0\"\nauthor=\"t\"\n\
             [[csv]]\nname=\"t.csv\"\nfile=\"gone.csv\"\nmode=\"rows\"\n",
        )
        .unwrap();
        let scanned = scan(&dir);
        assert!(scanned[0].manifest.as_ref().unwrap().csv[0].row_keys.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expands_asset_folder_recursive_deterministic_dirs() {
        let dir = tmpdir("assetfolder");
        std::fs::create_dir_all(dir.join("m/assets/train/mytrain/lod")).unwrap();
        std::fs::create_dir_all(dir.join("m/assets/train/other")).unwrap();
        std::fs::write(
            dir.join("m/mod.toml"),
            "id=\"m\"\nname=\"M\"\nversion=\"1.0.0\"\nauthor=\"t\"\n\
             [[asset]]\nfolder=\"assets\"\nhost=\"vp\"\n",
        )
        .unwrap();
        for (p, b) in [
            ("m/assets/train/mytrain/_t_a.x", b"a"),
            ("m/assets/train/mytrain/lod/_t_b.x", b"b"),
            ("m/assets/train/other/_t_c.att", b"c"),
            ("m/assets/x.dds", b"d"),
            ("m/assets/loose.dat", b"e"),
        ] {
            std::fs::write(dir.join(p), b).unwrap();
        }
        let scanned = scan(&dir);
        let m = scanned[0].manifest.as_ref().unwrap();
        let names: Vec<&str> = m.asset.iter().map(|a| a.name.as_str()).collect();
        // sorted by relative path
        assert_eq!(
            names,
            vec!["loose.dat", "_t_a.x", "_t_b.x", "_t_c.att", "x.dds"]
        );
        let by = |n: &str| m.asset.iter().find(|a| a.name == n).unwrap();
        // subfolder path relative to the folder IS the row dir
        assert_eq!(by("_t_a.x").dir, "train\\mytrain");
        assert_eq!(by("_t_b.x").dir, "train\\mytrain\\lod");
        assert_eq!(by("_t_c.att").dir, "train\\other");
        // a root file takes the folder's own path as its dir
        assert_eq!(by("x.dds").dir, "assets");
        assert_eq!(by("loose.dat").dir, "assets");
        // the payload path is mod-relative with '/'
        assert_eq!(by("_t_a.x").file, "assets/train/mytrain/_t_a.x");
        assert_eq!(by("loose.dat").file, "assets/loose.dat");
        // host/override carry over; every op is single-file form now
        assert!(m.asset.iter().all(|a| a.folder.is_none() && a.host == "vp"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn asset_folder_from_mod_root_uses_explicit_dir() {
        let dir = tmpdir("assetroot");
        std::fs::create_dir_all(dir.join("m/sub")).unwrap();
        std::fs::write(
            dir.join("m/mod.toml"),
            "id=\"m\"\nname=\"M\"\nversion=\"1.0.0\"\nauthor=\"t\"\n\
             [[asset]]\nfolder=\".\"\ndir=\"train\\\\room\"\nhost=\"auto\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("m/_t_r.x"), b"r").unwrap();
        std::fs::write(dir.join("m/sub/_t_s.x"), b"s").unwrap();
        let scanned = scan(&dir);
        let m = scanned[0].manifest.as_ref().unwrap();
        let by = |n: &str| m.asset.iter().find(|a| a.name == n).unwrap();
        // root file: folder path is empty -> explicit dir
        assert_eq!(by("_t_r.x").dir, "train\\room");
        // subfolder file: subfolder path relative to the folder
        assert_eq!(by("_t_s.x").dir, "sub");
        assert_eq!(m.asset[0].host, crate::manifest::HOST_AUTO);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn asset_folder_duplicate_basenames_are_a_hard_error() {
        let dir = tmpdir("assetdup");
        std::fs::create_dir_all(dir.join("m/assets/a")).unwrap();
        std::fs::create_dir_all(dir.join("m/assets/b")).unwrap();
        std::fs::write(
            dir.join("m/mod.toml"),
            "id=\"m\"\nname=\"M\"\nversion=\"1.0.0\"\nauthor=\"t\"\n\
             [[asset]]\nfolder=\"assets\"\nhost=\"vp\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("m/assets/a/_t_dup.x"), b"a").unwrap();
        std::fs::write(dir.join("m/assets/b/_t_dup.x"), b"b").unwrap();
        let scanned = scan(&dir);
        let e = scanned[0].manifest.as_ref().unwrap_err().to_string();
        assert!(e.contains("two files named") && e.contains("globally unique"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn asset_folder_duplicate_across_sections_is_refused() {
        let dir = tmpdir("assetdup2");
        std::fs::create_dir_all(dir.join("m/assets")).unwrap();
        std::fs::write(
            dir.join("m/mod.toml"),
            "id=\"m\"\nname=\"M\"\nversion=\"1.0.0\"\nauthor=\"t\"\n\
             [[asset]]\nfolder=\"assets\"\nhost=\"vp\"\n\
             [[asset]]\nname=\"_t_dup.x\"\ndir=\"train\\\\d\"\nhost=\"vp\"\nfile=\"f\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("m/assets/_t_dup.x"), b"a").unwrap();
        let scanned = scan(&dir);
        let e = scanned[0].manifest.as_ref().unwrap_err().to_string();
        assert!(e.contains("two [[asset]] sections for name"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_asset_folder_is_refused() {
        let dir = tmpdir("assetmissing");
        std::fs::create_dir_all(dir.join("m")).unwrap();
        std::fs::write(
            dir.join("m/mod.toml"),
            "id=\"m\"\nname=\"M\"\nversion=\"1.0.0\"\nauthor=\"t\"\n\
             [[asset]]\nfolder=\"nope\"\nhost=\"vp\"\n",
        )
        .unwrap();
        let scanned = scan(&dir);
        let e = scanned[0].manifest.as_ref().unwrap_err().to_string();
        assert!(e.contains("is not a directory"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_manifests_are_skipped_loudly_not_fatal() {
        let dir = tmpdir("mixed");
        std::fs::create_dir_all(dir.join("good")).unwrap();
        std::fs::create_dir_all(dir.join("bad")).unwrap();
        std::fs::create_dir_all(dir.join("future")).unwrap();
        std::fs::create_dir_all(dir.join("nomanifest")).unwrap();
        std::fs::write(
            dir.join("good/mod.toml"),
            "id=\"good\"\nname=\"G\"\nversion=\"1.0.0\"\nauthor=\"t\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("bad/mod.toml"), "id = 5\n").unwrap();
        std::fs::write(
            dir.join("future/mod.toml"),
            "id=\"f\"\nname=\"F\"\nversion=\"1.0.0\"\nauthor=\"t\"\n[native]\nabi=1\n",
        )
        .unwrap();
        let scanned = scan(&dir);
        // all four dirs report; a dir without mod.toml is a LOUD error entry
        assert_eq!(scanned.len(), 4);
        let nomanifest = scanned.iter().find(|s| s.dir.ends_with("nomanifest")).unwrap();
        assert!(nomanifest.manifest.is_err());
        let good = scanned.iter().find(|s| s.dir.ends_with("good")).unwrap();
        assert!(good.manifest.is_ok());
        let bad = scanned.iter().find(|s| s.dir.ends_with("bad")).unwrap();
        assert!(bad.manifest.is_err());
        let future = scanned.iter().find(|s| s.dir.ends_with("future")).unwrap();
        let err = future.manifest.as_ref().unwrap_err().to_string();
        assert!(err.contains("future loader version"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
