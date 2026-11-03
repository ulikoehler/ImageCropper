use imagecropper::app::saver::Saver;
use imagecropper::fs_utils::previews::{preview_sig_u64, previews_dir_for, produce_preview_bytes};
use imagecropper::fs_utils::TRASH_DIR;
use imagecropper::image_utils::OutputFormat;
use imagecropper::server::actions::{execute, verify_sig, Action, RotateOp};

mod common;
use common::{solid_image, with_temp_workdir, write_image};

#[test]
fn delete_moves_to_trash_and_purges_previews() {
    with_temp_workdir(|root| {
        let img = root.join("a.png");
        write_image(&img, &solid_image(40, 40, [9, 9, 9, 255]));
        produce_preview_bytes(&img, None).unwrap();
        assert!(previews_dir_for(&img).read_dir().unwrap().next().is_some());

        let sig = preview_sig_u64(&img).unwrap();
        let mut saver = Saver::new(1);
        let (_, bytes) = execute(&Action::Delete { id: 0, sig }, &img, &mut saver, None).unwrap();

        assert!(!img.exists());
        assert!(root.join(TRASH_DIR).join("a.png").exists());
        assert!(bytes.unwrap() > 0);
        assert!(
            previews_dir_for(&img)
                .read_dir()
                .map(|mut d| d.next().is_none())
                .unwrap_or(true),
            "previews purged with the file"
        );
    });
}

#[test]
fn stale_sig_rejects() {
    with_temp_workdir(|root| {
        let img = root.join("a.png");
        write_image(&img, &solid_image(10, 10, [1, 2, 3, 255]));
        assert!(verify_sig(&img, 0xDEADBEEF).is_err());
        let mut saver = Saver::new(1);
        let r = execute(&Action::Delete { id: 0, sig: 1 }, &img, &mut saver, None);
        assert!(r.is_err());
        assert!(img.exists(), "rejected action must not touch the file");
    });
}

#[test]
fn verify_sig_accepts_current() {
    with_temp_workdir(|root| {
        let img = root.join("a.png");
        write_image(&img, &solid_image(10, 10, [1, 2, 3, 255]));
        let sig = preview_sig_u64(&img).unwrap();
        assert!(verify_sig(&img, sig).is_ok());
    });
}

#[test]
fn crop_action_queues_save() {
    with_temp_workdir(|root| {
        let img = root.join("a.png");
        write_image(&img, &solid_image(100, 80, [50, 60, 70, 255]));
        let sig = preview_sig_u64(&img).unwrap();
        let mut saver = Saver::new(1);
        execute(
            &Action::Crop {
                id: 0,
                sig,
                sel: vec![[10, 10, 50, 40]],
                rotate: RotateOp::None,
                format: OutputFormat::Jpg,
                quality: 80,
            },
            &img,
            &mut saver,
            None,
        )
        .unwrap();
        // Wait for the async save: original moves to .imagecropper-originals,
        // cropped output lands at the same path.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let done = saver.check_completions();
            if !done.is_empty() {
                assert!(done[0].1.is_ok());
                break;
            }
            assert!(std::time::Instant::now() < deadline, "save timed out");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(img.exists(), "cropped output written to same path");
        let out = image::load_from_memory(&std::fs::read(&img).unwrap()).unwrap();
        assert_eq!((out.width(), out.height()), (50, 40), "cropped dimensions");
    });
}

#[test]
fn move_lands_in_move_dir_and_purges_previews() {
    with_temp_workdir(|root| {
        let img = root.join("b.png");
        write_image(&img, &solid_image(24, 24, [200, 100, 50, 255]));
        produce_preview_bytes(&img, None).unwrap();
        let dest = root.join("moved");
        std::fs::create_dir_all(&dest).unwrap();

        let sig = preview_sig_u64(&img).unwrap();
        let mut saver = Saver::new(1);
        let (_, bytes) =
            execute(&Action::Move { id: 0, sig }, &img, &mut saver, Some(&dest)).unwrap();

        assert!(!img.exists(), "source removed");
        assert!(dest.join("b.png").exists(), "file landed in move dir");
        assert!(bytes.unwrap() > 0);
        assert!(
            previews_dir_for(&img)
                .read_dir()
                .map(|mut d| d.next().is_none())
                .unwrap_or(true),
            "previews purged for moved file"
        );
    });
}

#[test]
fn move_without_dir_errors() {
    with_temp_workdir(|root| {
        let img = root.join("c.png");
        write_image(&img, &solid_image(8, 8, [1, 1, 1, 255]));
        let sig = preview_sig_u64(&img).unwrap();
        let mut saver = Saver::new(1);
        let r = execute(&Action::Move { id: 0, sig }, &img, &mut saver, None);
        assert!(r.is_err());
        assert!(img.exists());
    });
}

#[test]
fn move_already_in_dir_errors() {
    with_temp_workdir(|root| {
        let dest = root.join("moved");
        std::fs::create_dir_all(&dest).unwrap();
        let img = dest.join("d.png");
        write_image(&img, &solid_image(8, 8, [2, 2, 2, 255]));
        let sig = preview_sig_u64(&img).unwrap();
        let mut saver = Saver::new(1);
        let r = execute(&Action::Move { id: 0, sig }, &img, &mut saver, Some(&dest));
        assert!(r.is_err(), "moving a file onto itself is an error");
        assert!(img.exists());
    });
}
