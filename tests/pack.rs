//! The `thumbs.pack` flatfile store: one append-only log per previews dir
//! replaces per-image files. Records replay into an index on open;
//! tombstones and stale signatures survive restarts; a torn tail is cut.

use imagecropper::fs_utils::pack::{pack_for, Pack, PACK_FILE};
use tempfile::tempdir;

fn jpeg(n: u8) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8];
    v.extend(vec![n; 100]);
    v.extend([0xFF, 0xD9]);
    v
}

fn hash(n: u8) -> String {
    format!("{:064x}", n as u64 * 0x0101_0101_0101_0101)
}

#[test]
fn put_lookup_reopen() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    {
        let mut p = Pack::open(dir).unwrap();
        p.put("a.png", 100, 5, &hash(1), 256, &jpeg(7));
        p.put("b.png", 200, 6, &hash(2), 256, &jpeg(8));
        p.put_hash("c.png", 300, 7, &hash(3));
    }
    // Replay from disk in a fresh handle.
    let mut p = Pack::open(dir).unwrap();
    assert_eq!(p.lookup("a.png", 100, 5, 256).unwrap().0, jpeg(7));
    assert_eq!(p.lookup("b.png", 200, 6, 256).unwrap().0, jpeg(8));
    // Wrong signature → miss.
    assert!(p.lookup("a.png", 999, 5, 256).is_none());
    // Wrong produced-at dimension → miss.
    assert!(p.lookup("a.png", 100, 5, 512).is_none());
    // Hash-only record resolves as a hash, not a thumbnail.
    assert_eq!(p.stored_hash("c.png").as_deref(), Some(hash(3).as_str()));
    assert!(p.lookup("c.png", 300, 7, 256).is_none());
}

#[test]
fn tombstone_survives_reopen() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    {
        let mut p = Pack::open(dir).unwrap();
        p.put("a.png", 1, 1, &hash(1), 256, &jpeg(1));
        p.put("b.png", 1, 1, &hash(2), 256, &jpeg(2));
        p.remove("a.png");
        assert!(p.lookup("a.png", 1, 1, 256).is_none());
    }
    let mut p = Pack::open(dir).unwrap();
    assert!(p.lookup("a.png", 1, 1, 256).is_none());
    assert_eq!(p.lookup("b.png", 1, 1, 256).unwrap().0, jpeg(2));
    assert!(p.stored_hash("a.png").is_none());
}

#[test]
fn new_signature_prunes_old_variant() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let mut p = Pack::open(dir).unwrap();
    p.put("a.png", 1, 1, &hash(1), 256, &jpeg(1));
    p.put("a.png", 2, 2, &hash(2), 256, &jpeg(2));
    // Old signature is superseded — only the fresh variant resolves.
    assert!(p.lookup("a.png", 1, 1, 256).is_none());
    assert_eq!(p.lookup("a.png", 2, 2, 256).unwrap().0, jpeg(2));
    // …and the hash follows the newest record.
    assert_eq!(p.stored_hash("a.png").as_deref(), Some(hash(2).as_str()));
}

#[test]
fn torn_tail_is_truncated_on_open() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let good_len;
    {
        let mut p = Pack::open(dir).unwrap();
        p.put("a.png", 1, 1, &hash(1), 256, &jpeg(9));
        good_len = dir.join(PACK_FILE).metadata().unwrap().len();
        p.put("b.png", 2, 2, &hash(2), 256, &jpeg(8));
    }
    // Simulate a crash mid-append: chop the file inside the second record.
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(dir.join(PACK_FILE))
        .unwrap();
    f.set_len(good_len + 20).unwrap();
    drop(f);

    let mut p = Pack::open(dir).unwrap();
    assert_eq!(p.lookup("a.png", 1, 1, 256).unwrap().0, jpeg(9));
    // The torn record is gone — and appends still work afterwards.
    p.put("c.png", 3, 3, &hash(3), 256, &jpeg(3));
    drop(p);
    let mut p = Pack::open(dir).unwrap();
    assert_eq!(p.lookup("c.png", 3, 3, 256).unwrap().0, jpeg(3));
}

#[test]
fn registry_shares_and_empty_packs_are_unlinked() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path().join("prevs");
    let p1 = pack_for(&dir).unwrap();
    let p2 = pack_for(&dir).unwrap();
    assert!(std::sync::Arc::ptr_eq(&p1, &p2), "same dir → same pack");
    p1.lock().unwrap().put("a.png", 1, 1, &hash(1), 256, &jpeg(1));
    p1.lock().unwrap().remove("a.png");
    imagecropper::fs_utils::pack::remove_pack_if_empty(&dir);
    assert!(!dir.join(PACK_FILE).exists(), "empty pack must be unlinked");
    // A dead shared handle refuses writes instead of leaking records into
    // an unlinked file.
    p1.lock().unwrap().put("b.png", 1, 1, &hash(1), 256, &jpeg(1));
    let p3 = pack_for(&dir).unwrap();
    assert!(p3.lock().unwrap().lookup("b.png", 1, 1, 256).is_none());
}

#[test]
fn tombstones_accumulate_until_the_compaction_floor() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let mut p = Pack::open(dir).unwrap();
    for i in 0..4u8 {
        p.put(&format!("{i}.png"), 1, 1, &hash(i), 256, &jpeg(i));
    }
    let full = dir.join(PACK_FILE).metadata().unwrap().len();

    // Tombstoning appends; the file does NOT shrink below the 1 GiB floor.
    p.remove("0.png");
    p.remove("1.png");
    assert!(p.garbage_bytes() > 0, "tombstoned records count as garbage");
    assert_eq!(
        dir.join(PACK_FILE).metadata().unwrap().len(),
        full + 2 * (7 + "0.png".len()) as u64,
        "small amounts of garbage must not trigger a rewrite"
    );

    // Explicit compaction (what the 1 GiB check fires) reclaims the space
    // while every live record still resolves.
    p.compact_now();
    let compacted = dir.join(PACK_FILE).metadata().unwrap().len();
    assert!(compacted < full, "compaction must reclaim dead bytes");
    assert_eq!(p.garbage_bytes(), 0);
    assert_eq!(p.lookup("2.png", 1, 1, 256).unwrap().0, jpeg(2));
    assert_eq!(p.lookup("3.png", 1, 1, 256).unwrap().0, jpeg(3));
    // …and the rewritten file replays cleanly.
    drop(p);
    let mut p = Pack::open(dir).unwrap();
    assert_eq!(p.lookup("2.png", 1, 1, 256).unwrap().0, jpeg(2));
}

#[test]
fn fully_invalidated_pack_is_rewritten_empty() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let mut p = Pack::open(dir).unwrap();
    p.put("a.png", 1, 1, &hash(1), 256, &jpeg(1));
    p.remove("a.png");
    // All records invalidated → the pack is rewritten (empty), not left
    // holding dead bytes.
    assert_eq!(dir.join(PACK_FILE).metadata().unwrap().len(), 0);
    assert!(p.is_empty());
    // And it still accepts new records afterwards.
    p.put("b.png", 1, 1, &hash(2), 256, &jpeg(2));
    assert_eq!(p.lookup("b.png", 1, 1, 256).unwrap().0, jpeg(2));
}
