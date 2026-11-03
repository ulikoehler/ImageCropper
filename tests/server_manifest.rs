use imagecropper::fs_utils::previews::preview_sig_u64;
use imagecropper::server::{Manifest, ManifestItem, ManifestPatch, ServeMode};

mod common;
use common::{solid_image, with_temp_workdir, write_image};

#[test]
fn manifest_build_interns_dirs_and_sniffs_dims() {
    with_temp_workdir(|root| {
        write_image(root.join("a.png"), &solid_image(100, 50, [255, 0, 0, 255]));
        let sub = root.join("sub");
        std::fs::create_dir(&sub).unwrap();
        write_image(sub.join("b.png"), &solid_image(20, 30, [0, 255, 0, 255]));

        let files = vec![root.join("a.png"), sub.join("b.png")];
        let m = Manifest::build(&files, &[root.to_path_buf()], ServeMode::Album, 1);

        assert_eq!(m.items.len(), 2);
        assert_eq!(m.dirs.len(), 2); // interned
        assert_eq!((m.items[0].w, m.items[0].h), (100, 50));
        assert_eq!((m.items[1].w, m.items[1].h), (20, 30));
        assert_eq!(m.items[0].name, "a.png");
        assert_ne!(m.items[0].sig, 0);
        assert_eq!(m.mode, ServeMode::Album);
        assert_eq!(m.path_of(1).unwrap(), sub.join("b.png"));
    });
}

#[test]
fn manifest_sig_serializes_as_string() {
    with_temp_workdir(|root| {
        write_image(root.join("a.png"), &solid_image(10, 10, [0, 0, 0, 255]));
        let files = vec![root.join("a.png")];
        let m = Manifest::build(&files, &[root.to_path_buf()], ServeMode::Crop, 1);
        let json = serde_json::to_value(&m.items[0]).unwrap();
        assert!(json["sig"].is_string(), "sig must be a JSON string");
        assert_eq!(
            json["sig"].as_str().unwrap().parse::<u64>().unwrap(),
            preview_sig_u64(&root.join("a.png")).unwrap()
        );
        // Round-trip through JSON preserves the u64 exactly.
        let item: ManifestItem = serde_json::from_value(json).unwrap();
        assert_eq!(item.sig, m.items[0].sig);
    });
}

#[test]
fn manifest_patch_upsert_and_remove() {
    let mk = |id: u32, name: &str| ManifestItem {
        id,
        name: name.into(),
        dir_index: 0,
        w: 1,
        h: 1,
        bytes: 1,
        sig: 1,
        removed: false,
    };
    let mut m = Manifest {
        version: 1,
        mode: ServeMode::Album,
        generated_ms: 0,
        dirs: vec!["/d".into()],
        roots: vec![],
        items: vec![mk(0, "a"), mk(1, "b"), mk(2, "c")],
        positions: Default::default(),
    };
    m.rebuild_positions();
    let patch = ManifestPatch {
        version: 2,
        resync: false,
        upsert: vec![ManifestItem {
            w: 9,
            h: 9,
            bytes: 9,
            sig: 9,
            ..mk(1, "b2")
        }],
        remove: vec![0],
        dirs_added: vec!["/e".into()],
    };
    m.apply_patch(&patch);
    assert_eq!(m.version, 2);
    assert_eq!(m.dirs, vec!["/d", "/e"]);
    // Removals are tombstones — ids stay positional across clients.
    assert_eq!(m.items.len(), 3);
    assert!(m.items[0].removed);
    assert!(m.path_of(0).is_none());
    assert_eq!(m.items[1].name, "b2");
    assert_eq!((m.items[1].w, m.items[1].sig), (9, 9));
    assert_eq!(m.items[2].name, "c");
}
