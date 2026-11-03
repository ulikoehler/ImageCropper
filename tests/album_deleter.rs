use std::time::{Duration, Instant};

use imagecropper::album::deleter::Deleter;
use imagecropper::fs_utils::previews::{previews_dir_for, produce_preview_bytes};
use imagecropper::fs_utils::TRASH_DIR;

mod common;
use common::{solid_image, with_temp_workdir, write_image};

#[test]
fn deleter_trashes_bulk_and_purges_previews() {
    with_temp_workdir(|root| {
        let mut paths = Vec::new();
        for i in 0..40u8 {
            let path = root.join(format!("img{i:02}.png"));
            write_image(&path, &solid_image(16, 16, [i, 0, 0, 255]));
            produce_preview_bytes(&path, None).expect("preview");
            paths.push(path);
        }
        assert!(previews_dir_for(&paths[0])
            .read_dir()
            .unwrap()
            .next()
            .is_some());

        let mut deleter = Deleter::new(32);
        for path in &paths {
            deleter.queue(path.clone());
        }
        assert_eq!(deleter.pending.len(), paths.len());

        let deadline = Instant::now() + Duration::from_secs(20);
        let mut done = 0;
        while done < paths.len() && Instant::now() < deadline {
            done += deleter.poll().len();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(done, paths.len(), "all deletions completed");
        assert!(deleter.pending.is_empty());

        for path in &paths {
            assert!(!path.exists(), "{} should be trashed", path.display());
        }
        let trash = root.join(TRASH_DIR);
        assert_eq!(std::fs::read_dir(&trash).unwrap().count(), paths.len());
        let remaining: Vec<_> = std::fs::read_dir(previews_dir_for(&paths[0]))
            .unwrap()
            .flatten()
            .collect();
        assert!(
            remaining.is_empty(),
            "batched cleanup must purge every preview"
        );
    });
}
