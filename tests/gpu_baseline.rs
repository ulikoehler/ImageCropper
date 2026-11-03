// GPU JPEG decode+resize regression: byte-exact hashes recorded from
// the former C++/Vulkan implementation (AMD RADV). The pure-Rust wgpu
// port must reproduce them bit-for-bit.
use imagecropper::gpujpeg;
use std::process::{Command, Stdio};

fn hash(b: &[u8]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher;
    let mut h = DefaultHasher::new();
    h.write(b);
    format!("{:016x}", h.finish())
}

fn grad_ppm(w: usize, h: usize) -> Vec<u8> {
    let mut v = format!("P6\n{w} {h}\n255\n").into_bytes();
    for y in 0..h {
        for x in 0..w {
            v.extend_from_slice(&[
                (x * 3 % 256) as u8,
                (y * 5 % 256) as u8,
                ((x * 7 + y * 11) % 256) as u8,
            ]);
        }
    }
    v
}
fn photo_ppm(w: usize, h: usize) -> Vec<u8> {
    let mut v = format!("P6\n{w} {h}\n255\n").into_bytes();
    for y in 0..h {
        for x in 0..w {
            let n = (x * 31 ^ y * 17 ^ (x * y) % 251) % 256;
            let s = if (x / 40 + y / 30) % 2 == 0 { 255 } else { 0 };
            v.extend_from_slice(&[((n + s) / 2) as u8, ((x * 2 + y) % 256) as u8, n as u8]);
        }
    }
    v
}
fn cjpeg(ppm: &[u8], args: &[&str]) -> Option<Vec<u8>> {
    // file-arg invocation — piping >64KB through stdin deadlocks
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let f = std::env::temp_dir().join(format!(
        "ic-base-{}-{}.ppm",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&f, ppm).unwrap();
    let out = Command::new("cjpeg")
        .args(args)
        .arg("-outfile")
        .arg("/dev/stdout")
        .arg(&f)
        .stdout(Stdio::piped())
        .output()
        .ok()?;
    let _ = std::fs::remove_file(&f);
    out.stdout.starts_with(&[0xFF, 0xD8]).then_some(out.stdout)
}

struct Case {
    name: &'static str,
    ppm: Vec<u8>,
    args: &'static [&'static str],
    dec: &'static str,
    rs: &'static [(&'static str, u32, u32)], // (hash, max_w, max_h)
}

#[test]
fn baseline_hashes_match_cpp_impl() {
    if !gpujpeg::available() {
        eprintln!("gpujpeg unavailable — skipping");
        return;
    }
    let cases: Vec<Case> = vec![
        Case {
            name: "r4_420_photo",
            ppm: photo_ppm(801, 601),
            args: &["-restart", "4", "-quality", "85"],
            dec: "f0df2c42f9fba34a",
            rs: &[
                ("5458580698c45540", 512, 512),
                ("cb97cb4ba90c1e4a", 1920, 1920),
                ("da61252ddd0bb1ac", 100, 77),
            ],
        },
        Case {
            name: "r8_444_grad",
            ppm: grad_ppm(640, 480),
            args: &["-restart", "8", "-quality", "95", "-sample", "1x1"],
            dec: "7f24b0d45d7b3607",
            rs: &[
                ("07ea15a696777fa9", 512, 512),
                ("d9a5132cda2c6d59", 1920, 1920),
                ("94b4ed6cf01fe833", 100, 77),
            ],
        },
        Case {
            name: "r2_gray",
            ppm: {
                let mut g = format!("P5\n300 200\n255\n").into_bytes();
                for i in 0..300 * 200 {
                    g.push((i * 7 % 256) as u8);
                }
                g
            },
            args: &["-restart", "2", "-quality", "90", "-grayscale"],
            dec: "c0157952a893d3bf",
            rs: &[
                ("2742733a978aed92", 512, 512),
                ("2742733a978aed92", 1920, 1920),
                ("f039bbe2b3491494", 100, 77),
            ],
        },
        Case {
            name: "r4_422",
            ppm: photo_ppm(511, 399),
            args: &["-restart", "4", "-quality", "75", "-sample", "2x1"],
            dec: "a1d9de6225243ae2",
            rs: &[
                ("2740ea54830c951a", 512, 512),
                ("2740ea54830c951a", 1920, 1920),
                ("22006323db383b08", 100, 77),
            ],
        },
        Case {
            name: "r16_small",
            ppm: grad_ppm(96, 80),
            args: &["-restart", "16", "-quality", "50"],
            dec: "340714819bfc3bbd",
            rs: &[
                ("1998524833d3ce08", 512, 512),
                ("1998524833d3ce08", 1920, 1920),
                ("95809f2471da5990", 100, 77),
            ],
        },
        Case {
            name: "r4_420_big",
            ppm: photo_ppm(1920, 1080),
            args: &["-restart", "4", "-quality", "88"],
            dec: "140e9d6f3cde8130",
            rs: &[
                ("cfa37b8b49ffc8b6", 512, 512),
                ("edcb986bd0f8595b", 1920, 1920),
                ("c6117fcb4c7b7615", 100, 77),
            ],
        },
    ];
    let mut checked = 0;
    for c in &cases {
        let Some(jpg) = cjpeg(&c.ppm, c.args) else {
            eprintln!("{}: cjpeg unavailable — skipping", c.name);
            continue;
        };
        let (rgba, _, _) = gpujpeg::decode_rgba(&jpg).expect("decode_rgba");
        assert_eq!(hash(&rgba), c.dec, "{} decode hash", c.name);
        checked += 1;
        let s = gpujpeg::open(&jpg).expect("open");
        for (want, mw, mh) in c.rs {
            let (rgb, _, _) = s.resize_rgb(*mw, *mh).expect("resize_rgb");
            assert_eq!(hash(&rgb), *want, "{} resize {mw}x{mh}", c.name);
            checked += 1;
        }
    }
    assert!(checked > 0, "no cases ran (cjpeg missing?)");
}

/// Progressive (SOF2) versions of the baseline fixtures. Same source
/// pixels + same encoder settings produce the same quantized
/// coefficients, just packaged as multi-scan spectral-selection/
/// successive-approximation scans — so the expected hashes are the
/// SAME as the baseline cases above: a built-in cross-check that the
/// progressive scan machinery reproduces identical pixels.
#[test]
fn progressive_hashes_match_cpp_impl() {
    if !gpujpeg::available() {
        eprintln!("gpujpeg unavailable — skipping");
        return;
    }
    let cases: Vec<Case> = vec![
        Case {
            name: "prog_420_photo",
            ppm: photo_ppm(801, 601),
            args: &["-progressive", "-restart", "4", "-quality", "85"],
            dec: "f0df2c42f9fba34a",
            rs: &[
                ("5458580698c45540", 512, 512),
                ("cb97cb4ba90c1e4a", 1920, 1920),
                ("da61252ddd0bb1ac", 100, 77),
            ],
        },
        Case {
            name: "prog_444_grad",
            ppm: grad_ppm(640, 480),
            args: &[
                "-progressive",
                "-restart",
                "2",
                "-quality",
                "95",
                "-sample",
                "1x1",
            ],
            dec: "7f24b0d45d7b3607",
            rs: &[
                ("07ea15a696777fa9", 512, 512),
                ("d9a5132cda2c6d59", 1920, 1920),
                ("94b4ed6cf01fe833", 100, 77),
            ],
        },
        Case {
            name: "prog_gray",
            ppm: {
                let mut g = format!("P5\n300 200\n255\n").into_bytes();
                for i in 0..300 * 200 {
                    g.push((i * 7 % 256) as u8);
                }
                g
            },
            args: &[
                "-progressive",
                "-restart",
                "8",
                "-quality",
                "90",
                "-grayscale",
            ],
            dec: "c0157952a893d3bf",
            rs: &[
                ("2742733a978aed92", 512, 512),
                ("2742733a978aed92", 1920, 1920),
                ("f039bbe2b3491494", 100, 77),
            ],
        },
        Case {
            name: "prog_422",
            ppm: photo_ppm(511, 399),
            args: &[
                "-progressive",
                "-restart",
                "4",
                "-quality",
                "75",
                "-sample",
                "2x1",
            ],
            dec: "a1d9de6225243ae2",
            rs: &[
                ("2740ea54830c951a", 512, 512),
                ("2740ea54830c951a", 1920, 1920),
                ("22006323db383b08", 100, 77),
            ],
        },
        Case {
            name: "prog_small",
            ppm: grad_ppm(96, 80),
            args: &["-progressive", "-restart", "2", "-quality", "50"],
            dec: "340714819bfc3bbd",
            rs: &[
                ("1998524833d3ce08", 512, 512),
                ("1998524833d3ce08", 1920, 1920),
                ("95809f2471da5990", 100, 77),
            ],
        },
        Case {
            name: "prog_420_big",
            ppm: photo_ppm(1920, 1080),
            args: &["-progressive", "-restart", "8", "-quality", "88"],
            dec: "140e9d6f3cde8130",
            rs: &[
                ("cfa37b8b49ffc8b6", 512, 512),
                ("edcb986bd0f8595b", 1920, 1920),
                ("c6117fcb4c7b7615", 100, 77),
            ],
        },
    ];
    let mut checked = 0;
    for c in &cases {
        let Some(jpg) = cjpeg(&c.ppm, c.args) else {
            eprintln!("{}: cjpeg unavailable — skipping", c.name);
            continue;
        };
        // sanity: fixture really is progressive (SOF2)
        assert!(
            jpg.windows(2).any(|w| w == [0xFF, 0xC2]),
            "{} not SOF2",
            c.name
        );
        let (rgba, _, _) = gpujpeg::decode_rgba(&jpg).expect("decode_rgba");
        assert_eq!(hash(&rgba), c.dec, "{} decode hash", c.name);
        checked += 1;
        let s = gpujpeg::open(&jpg).expect("open");
        for (want, mw, mh) in c.rs {
            let (rgb, _, _) = s.resize_rgb(*mw, *mh).expect("resize_rgb");
            assert_eq!(hash(&rgb), *want, "{} resize {mw}x{mh}", c.name);
            checked += 1;
        }
    }
    assert!(checked > 0, "no cases ran (cjpeg missing?)");
}

/// DRI=0 fixtures (no restart markers — the common real-world case).
/// The survey pass splits each serial scan into virtual 256-MCU
/// intervals with host-recorded decoder state; progressive DRI=0 files
/// additionally exercise the deferred AC-refine survey (coef readback
/// between submits). Same source + quality => same coefficients => the
/// SAME expected hashes as the restart-marker cases.
#[test]
fn dri0_hashes_match_cpp_impl() {
    if !gpujpeg::available() {
        eprintln!("gpujpeg unavailable — skipping");
        return;
    }
    let cases: Vec<Case> = vec![
        Case {
            name: "norst_photo", // >256 MCUs => surveyed baseline scan
            ppm: photo_ppm(801, 601),
            args: &["-quality", "85"],
            dec: "f0df2c42f9fba34a",
            rs: &[
                ("5458580698c45540", 512, 512),
                ("cb97cb4ba90c1e4a", 1920, 1920),
                ("da61252ddd0bb1ac", 100, 77),
            ],
        },
        Case {
            name: "norst_grad444",
            ppm: grad_ppm(640, 480),
            args: &["-quality", "95", "-sample", "1x1"],
            dec: "7f24b0d45d7b3607",
            rs: &[
                ("07ea15a696777fa9", 512, 512),
                ("d9a5132cda2c6d59", 1920, 1920),
                ("94b4ed6cf01fe833", 100, 77),
            ],
        },
        Case {
            name: "norst_gray",
            ppm: {
                let mut g = format!("P5\n300 200\n255\n").into_bytes();
                for i in 0..300 * 200 {
                    g.push((i * 7 % 256) as u8);
                }
                g
            },
            args: &["-quality", "90", "-grayscale"],
            dec: "c0157952a893d3bf",
            rs: &[
                ("2742733a978aed92", 512, 512),
                ("2742733a978aed92", 1920, 1920),
                ("f039bbe2b3491494", 100, 77),
            ],
        },
        Case {
            name: "norst_prog", // deferred AC-refine surveys
            ppm: photo_ppm(801, 601),
            args: &["-quality", "85", "-progressive"],
            dec: "f0df2c42f9fba34a",
            rs: &[
                ("5458580698c45540", 512, 512),
                ("cb97cb4ba90c1e4a", 1920, 1920),
                ("da61252ddd0bb1ac", 100, 77),
            ],
        },
        Case {
            name: "norst_prog_big", // multi-submit deferred surveys
            ppm: photo_ppm(1920, 1080),
            args: &["-quality", "88", "-progressive"],
            dec: "140e9d6f3cde8130",
            rs: &[
                ("cfa37b8b49ffc8b6", 512, 512),
                ("edcb986bd0f8595b", 1920, 1920),
                ("c6117fcb4c7b7615", 100, 77),
            ],
        },
    ];
    let mut checked = 0;
    for c in &cases {
        let Some(jpg) = cjpeg(&c.ppm, c.args) else {
            eprintln!("{}: cjpeg unavailable — skipping", c.name);
            continue;
        };
        assert!(
            !jpg.windows(2).any(|w| matches!(w, [0xFF, 0xDD])),
            "{} unexpectedly has a DRI marker",
            c.name
        );
        let (rgba, _, _) = gpujpeg::decode_rgba(&jpg).expect("decode_rgba");
        assert_eq!(hash(&rgba), c.dec, "{} decode hash", c.name);
        checked += 1;
        let s = gpujpeg::open(&jpg).expect("open");
        for (want, mw, mh) in c.rs {
            let (rgb, _, _) = s.resize_rgb(*mw, *mh).expect("resize_rgb");
            assert_eq!(hash(&rgb), *want, "{} resize {mw}x{mh}", c.name);
            checked += 1;
        }
    }
    assert!(checked > 0, "no cases ran (cjpeg missing?)");
}
