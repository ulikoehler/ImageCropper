use clap::Parser;
use imagecropper::cli::{Cli, Commands, SortOrder};
use std::net::SocketAddr;

#[test]
fn album_serve_and_no_gui_parse() {
    let cli = Cli::try_parse_from([
        "imagecropper",
        "album",
        "--serve=0.0.0.0:12345",
        "-G",
        "-p",
        "abc",
        "-r",
        "/photos",
    ])
    .unwrap();
    let Commands::Album(a) = cli.command else {
        panic!("expected album")
    };
    assert_eq!(
        a.headless.serve,
        Some("0.0.0.0:12345".parse::<SocketAddr>().unwrap())
    );
    assert!(a.headless.no_gui);
    assert_eq!(a.headless.password.as_deref(), Some("abc"));
    assert!(a.scan.recursive);
}

#[test]
fn crop_serve_parses() {
    let cli = Cli::try_parse_from([
        "imagecropper",
        "crop",
        "--serve=127.0.0.1:8737",
        "-q",
        "85",
        "-f",
        "jpg",
        "/x",
    ])
    .unwrap();
    let Commands::Crop(c) = cli.command else {
        panic!("expected crop")
    };
    assert_eq!(
        c.headless.serve,
        Some("127.0.0.1:8737".parse::<SocketAddr>().unwrap())
    );
    assert_eq!(c.quality, 85);
    assert!(!c.headless.no_gui);
}

#[test]
fn push_credit_default() {
    let cli = Cli::try_parse_from(["imagecropper", "album", "/x"]).unwrap();
    let Commands::Album(a) = cli.command else {
        panic!()
    };
    assert_eq!(a.headless.push_credit_kb, 4096);
    assert!(a.headless.serve.is_none());
}

#[test]
fn auth_defaults_to_token_and_parses_modes() {
    use imagecropper::cli::ServeAuth;
    let cli = Cli::try_parse_from(["imagecropper", "album", "/x"]).unwrap();
    let Commands::Album(a) = cli.command else {
        panic!()
    };
    assert_eq!(a.headless.auth, ServeAuth::Token);
    assert!(a.headless.password.is_none());

    let cli = Cli::try_parse_from([
        "imagecropper",
        "album",
        "--auth=none",
        "--serve=127.0.0.1:9",
        "/x",
    ])
    .unwrap();
    let Commands::Album(a) = cli.command else {
        panic!()
    };
    assert_eq!(a.headless.auth, ServeAuth::None);

    let cli = Cli::try_parse_from([
        "imagecropper",
        "album",
        "--auth=password",
        "-p",
        "hunter2",
        "/x",
    ])
    .unwrap();
    let Commands::Album(a) = cli.command else {
        panic!()
    };
    assert_eq!(a.headless.auth, ServeAuth::Password);
    assert_eq!(a.headless.password.as_deref(), Some("hunter2"));

    assert!(Cli::try_parse_from(["imagecropper", "album", "--auth=bogus", "/x"]).is_err());
}

#[test]
fn album_move_dir_parses() {
    let cli = Cli::try_parse_from(["imagecropper", "album", "-m", "/tmp/moved", "/x"]).unwrap();
    let Commands::Album(a) = cli.command else {
        panic!("expected album")
    };
    assert_eq!(
        a.move_dir.as_deref(),
        Some(std::path::Path::new("/tmp/moved"))
    );

    // -m is album-only
    let err = Cli::try_parse_from(["imagecropper", "crop", "-m", "/tmp/m", "/x"]);
    assert!(err.is_err(), "crop must not accept -m");
}

#[test]
fn resolve_move_dir_creates_and_validates() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("new_dir/nested");
    let resolved = imagecropper::cli::resolve_move_dir(&Some(dir.clone())).unwrap();
    assert!(resolved.unwrap().is_dir(), "missing dir created");

    let file = tmp.path().join("afile");
    std::fs::write(&file, b"x").unwrap();
    assert!(imagecropper::cli::resolve_move_dir(&Some(file)).is_err());

    assert!(imagecropper::cli::resolve_move_dir(&None)
        .unwrap()
        .is_none());
}

#[test]
fn order_values_still_parse() {
    for v in [
        "size",
        "size+",
        "size-",
        "exif",
        "exif+",
        "exif-",
        "modified",
        "randomize",
        "filename",
    ] {
        Cli::try_parse_from(["imagecropper", "album", "-o", v, "/x"])
            .unwrap_or_else(|e| panic!("-o {v} failed: {e}"));
    }
    let cli = Cli::try_parse_from(["imagecropper", "album", "-o", "size-", "/x"]).unwrap();
    let Commands::Album(a) = cli.command else {
        panic!()
    };
    assert_eq!(a.scan.order, SortOrder::SizeDesc);
}

#[test]
fn mouse_select_mode_defaults_to_sequence() {
    use imagecropper::cli::MouseSelectMode;
    let cli = Cli::try_parse_from(["imagecropper", "album", "/x"]).unwrap();
    let Commands::Album(a) = cli.command else {
        panic!()
    };
    assert_eq!(a.mouse_select, MouseSelectMode::Sequence);

    let cli = Cli::try_parse_from([
        "imagecropper",
        "album",
        "--mouse-select-mode=rubber-band",
        "/x",
    ])
    .unwrap();
    let Commands::Album(a) = cli.command else {
        panic!()
    };
    assert_eq!(a.mouse_select, MouseSelectMode::RubberBand);

    let cli = Cli::try_parse_from([
        "imagecropper",
        "album",
        "--mouse-select-mode=sequence",
        "/x",
    ])
    .unwrap();
    let Commands::Album(a) = cli.command else {
        panic!()
    };
    assert_eq!(a.mouse_select, MouseSelectMode::Sequence);

    assert!(
        Cli::try_parse_from(["imagecropper", "album", "--mouse-select-mode=bogus", "/x"]).is_err()
    );
}
