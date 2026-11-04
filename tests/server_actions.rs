use imagecropper::app::saver::Saver;
use imagecropper::fs_utils::previews::{preview_sig_u64, previews_dir_for, produce_preview_bytes};
use imagecropper::fs_utils::TRASH_DIR;
use imagecropper::image_utils::OutputFormat;
use imagecropper::server::actions::{execute, verify_sig, Action, ActionQueue, RotateOp};

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
        // A recorded content hash is what makes a signature decision exact
        // (producing the preview writes it). Without one the check cannot
        // tell mtime churn from a real change and records the hash instead
        // of blocking the action.
        imagecropper::fs_utils::previews::produce_preview_bytes(&img, None).expect("preview");
        let old_sig = preview_sig_u64(&img).unwrap();

        // Real content change → rejected, file untouched.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write_image(&img, &solid_image(20, 12, [9, 9, 9, 255]));
        assert!(verify_sig(&img, old_sig).is_err());
        let mut saver = Saver::new(1);
        let r = execute(&Action::Delete { id: 0, sig: old_sig }, &img, &mut saver, None);
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

#[test]
fn action_queue_deletes_bulk_in_parallel() {
    with_temp_workdir(|root| {
        let mut jobs = Vec::new();
        for i in 0..40u32 {
            let img = root.join(format!("bulk{i:02}.png"));
            write_image(&img, &solid_image(8, 8, [i as u8, 0, 0, 255]));
            produce_preview_bytes(&img, None).unwrap();
            let sig = preview_sig_u64(&img).unwrap();
            jobs.push((img, sig));
        }

        let queue = ActionQueue::new(1, 32, None);
        for (i, (path, sig)) in jobs.iter().enumerate() {
            queue.dispatch(
                i as u32,
                Action::Delete {
                    id: i as u32,
                    sig: *sig,
                },
                path.clone(),
            );
        }
        assert_eq!(queue.pending(), jobs.len());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut done = Vec::new();
        while done.len() < jobs.len() && std::time::Instant::now() < deadline {
            done.extend(queue.poll());
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(done.len(), jobs.len(), "all delete actions completed");
        assert!(done.iter().all(|d| d.ok), "all deletes succeeded");
        assert_eq!(queue.pending(), 0);
        for (path, _) in &jobs {
            assert!(!path.exists(), "{} should be trashed", path.display());
        }
        // The batched purge runs on its own thread — wait for it to drain.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let remaining: Vec<_> = std::fs::read_dir(previews_dir_for(&jobs[0].0))
                .unwrap()
                .flatten()
                .collect();
            if remaining.is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "batched cleanup must purge every preview"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    });
}

/// Deletes are queued and always finished: every dispatched tag gets exactly
/// one completion and `pending()` drains, so a slow filesystem delays work
/// instead of failing it.
#[test]
fn queued_deletes_all_finish() {
    with_temp_workdir(|root| {
        // One worker, many jobs: the queue is the thing under test.
        let queue = ActionQueue::new(1, 1, None);
        let mut tags = Vec::new();
        for i in 0..50u32 {
            let img = root.join(format!("queued{i:02}.png"));
            write_image(&img, &solid_image(8, 8, [i as u8, 1, 1, 255]));
            let sig = preview_sig_u64(&img).unwrap();
            queue.dispatch(i, Action::Delete { id: i, sig }, img);
            tags.push(i);
        }
        assert_eq!(queue.pending(), tags.len());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut seen: Vec<u32> = Vec::new();
        while queue.pending() > 0 && std::time::Instant::now() < deadline {
            for done in queue.poll() {
                assert!(
                    !seen.contains(&done.tag),
                    "tag {} completed twice",
                    done.tag
                );
                assert!(done.ok, "queued delete must succeed: {:?}", done.error);
                seen.push(done.tag);
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(queue.pending(), 0, "the queue must always drain");
        assert_eq!(seen.len(), tags.len(), "every dispatched op completed once");
        seen.sort_unstable();
        assert_eq!(seen, tags);
    });
}

/// Moves take precedence over queued deletes: with a single worker and a
/// delete already in flight, a move dispatched after another delete must
/// still complete before that queued delete runs.
#[test]
fn moves_jump_ahead_of_queued_deletes() {
    with_temp_workdir(|root| {
        let move_dir = root.join("moved");
        std::fs::create_dir(&move_dir).unwrap();
        let queue = ActionQueue::new(1, 1, Some(move_dir.clone()));

        let mk = |name: &str| {
            let img = root.join(name);
            write_image(&img, &solid_image(8, 8, [7, 7, 7, 255]));
            (img.clone(), preview_sig_u64(&img).unwrap())
        };
        let (d1, d1sig) = mk("d1.png");
        let (d2, d2sig) = mk("d2.png");
        let (m1, m1sig) = mk("m1.png");

        queue.dispatch(1, Action::Delete { id: 1, sig: d1sig }, d1.clone());
        // Let the worker dequeue d1 so d2 and m1 are definitely still
        // queued behind it.
        std::thread::sleep(std::time::Duration::from_millis(100));
        queue.dispatch(2, Action::Delete { id: 2, sig: d2sig }, d2.clone());
        queue.dispatch(3, Action::Move { id: 3, sig: m1sig }, m1.clone());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut order: Vec<u32> = Vec::new();
        while order.len() < 3 && std::time::Instant::now() < deadline {
            for done in queue.poll() {
                assert!(done.ok, "job failed: {:?}", done.error);
                order.push(done.tag);
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(order.len(), 3, "all three jobs completed");
        let pos_move = order.iter().position(|&t| t == 3).unwrap();
        let pos_d2 = order.iter().position(|&t| t == 2).unwrap();
        assert!(
            pos_move < pos_d2,
            "queued move must complete before queued delete: {order:?}"
        );
        assert!(move_dir.join("m1.png").exists(), "move landed");
        assert_eq!(queue.pending(), 0);
    });
}

/// Background deletes yield to live client requests but still finish: the
/// server must not let a busy client starve the queue.
#[test]
fn deletes_yield_to_client_work_and_still_finish() {
    with_temp_workdir(|root| {
        let gate = imagecropper::server::IoGate::new();
        let queue = ActionQueue::with_gate(1, 4, None, gate.clone());
        let img = root.join("yield.png");
        write_image(&img, &solid_image(8, 8, [3, 3, 3, 255]));
        let sig = preview_sig_u64(&img).unwrap();

        // A client request is in flight.
        let client = gate.client_work();
        queue.dispatch(1, Action::Delete { id: 0, sig }, img.clone());

        // The delete holds off while the client is being served.
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(queue.poll().is_empty(), "delete must wait for the client");
        assert!(img.exists());

        // Once the client is done, the queued delete lands promptly.
        drop(client);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut done = Vec::new();
        while done.is_empty() && std::time::Instant::now() < deadline {
            done.extend(queue.poll());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(done.len(), 1);
        assert!(done[0].ok, "delete failed: {:?}", done[0].error);
        assert!(!img.exists());
        assert_eq!(queue.pending(), 0);
    });
}
