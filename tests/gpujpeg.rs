//! GPU JPEG decoder integration: restart-marker baseline JPEGs decode
//! on the Vulkan path, everything else falls back to zune.
//! Requires `cjpeg` to synthesize restart-marker fixtures; skips if absent.

use imagecropper::gpujpeg;
use std::io::Write;
use std::process::{Command, Stdio};

fn gradient_ppm(w: usize, h: usize) -> Vec<u8> {
    let mut v = format!("P6\n{w} {h}\n255\n").into_bytes();
    for y in 0..h {
        for x in 0..w {
            v.extend_from_slice(&[
                (x * 3 % 256) as u8,
                (y * 5 % 256) as u8,
                ((x + y) % 256) as u8,
            ]);
        }
    }
    v
}

fn cjpeg(ppm: &[u8], args: &[&str]) -> Option<Vec<u8>> {
    let mut ch = Command::new("cjpeg")
        .args(args)
        .arg("-outfile")
        .arg("/dev/stdout")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    ch.stdin.as_mut()?.write_all(ppm).ok()?;
    let out = ch.wait_with_output().ok()?;
    (out.status.success() && out.stdout.starts_with(&[0xFF, 0xD8])).then_some(out.stdout)
}

#[test]
fn restart_jpeg_decodes_on_gpu() {
    if !gpujpeg::available() {
        eprintln!("gpujpeg not compiled in — skipping");
        return;
    }
    let Some(jpg) = cjpeg(&gradient_ppm(96, 80), &["-restart", "4", "-quality", "90"]) else {
        eprintln!("cjpeg missing — skipping");
        return;
    };

    let (w, h, dri) = gpujpeg::probe(&jpg).expect("probe baseline");
    assert_eq!((w, h), (96, 80));
    assert!(dri > 0, "expected restart interval");

    let (rgba, dw, dh) = gpujpeg::decode_rgba(&jpg).expect("gpu decode");
    assert_eq!((dw, dh), (96, 80));

    // zune is not bit-exact vs libjpeg — allow small rounding diffs
    let mut dec = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&jpg));
    let zrgb = dec.decode().expect("zune decode");
    assert_eq!(zrgb.len(), rgba.len() / 4 * 3);
    let mut worst = 0i32;
    for i in 0..zrgb.len() / 3 {
        for c in 0..3 {
            worst = worst.max((zrgb[i * 3 + c] as i32 - rgba[i * 4 + c] as i32).abs());
        }
    }
    assert!(worst <= 4, "gpu vs zune max diff {worst}");
}

#[test]
fn session_resize_matches_decode() {
    if !gpujpeg::available() {
        eprintln!("gpujpeg not compiled in — skipping");
        return;
    }
    let Some(jpg) = cjpeg(
        &gradient_ppm(320, 240),
        &["-restart", "4", "-quality", "92"],
    ) else {
        eprintln!("cjpeg missing — skipping");
        return;
    };

    let Some(s) = gpujpeg::open(&jpg) else {
        panic!("session open failed");
    };
    assert_eq!((s.width, s.height), (320, 240));

    // Identity-size resize packs the decoded words — bit-exact vs the
    // plain decode path's RGBA (minus the alpha byte).
    let (rgb, w, h) = s.resize_rgb(320, 240).expect("identity resize");
    assert_eq!((w, h), (320, 240));
    let (rgba, _, _) = gpujpeg::decode_rgba(&jpg).expect("gpu decode");
    for i in 0..320 * 240 {
        for c in 0..3 {
            assert_eq!(rgb[i * 3 + c], rgba[i * 4 + c], "px {i} ch {c}");
        }
    }

    // A downscale: right dims, plausible pixels (smooth gradient —
    // every output pixel stays within the input's value range).
    let (rgb, w, h) = s.resize_rgb(64, 64).expect("downscale");
    assert_eq!((w, h), (64, 48));
    assert_eq!(rgb.len(), 64 * 48 * 3);
    assert!(rgb.iter().any(|&v| v > 200) && rgb.iter().any(|&v| v < 50));
}

#[test]
fn dri0_decodes_on_gpu() {
    // plain cjpeg output has no restart markers; >256 MCUs means the
    // survey pass kicks in and synthesizes virtual restart intervals
    let Some(jpg) = cjpeg(&gradient_ppm(512, 512), &["-quality", "90"]) else {
        eprintln!("cjpeg missing — skipping");
        return;
    };
    if !gpujpeg::available() {
        eprintln!("gpujpeg not compiled in — skipping");
        return;
    }
    let (_, _, dri) = gpujpeg::probe(&jpg).expect("probe");
    assert_eq!(dri, 0, "expected DRI=0");

    let (rgba, dw, dh) = gpujpeg::decode_rgba(&jpg).expect("gpu decode");
    assert_eq!((dw, dh), (512, 512));

    let mut dec = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&jpg));
    let zrgb = dec.decode().expect("zune decode");
    let mut worst = 0i32;
    for i in 0..zrgb.len() / 3 {
        for c in 0..3 {
            worst = worst.max((zrgb[i * 3 + c] as i32 - rgba[i * 4 + c] as i32).abs());
        }
    }
    assert!(worst <= 4, "gpu vs zune max diff {worst}");
}

#[test]
fn dri0_progressive_decodes_on_gpu() {
    // DRI=0 progressive: surveys cover DC + AC-first scans upfront and
    // AC-refine scans deferred (coef readback between submits)
    let Some(jpg) = cjpeg(
        &gradient_ppm(512, 512),
        &["-quality", "90", "-progressive"],
    ) else {
        eprintln!("cjpeg missing — skipping");
        return;
    };
    if !gpujpeg::available() {
        eprintln!("gpujpeg not compiled in — skipping");
        return;
    }
    let (rgba, dw, dh) = gpujpeg::decode_rgba(&jpg).expect("gpu decode");
    assert_eq!((dw, dh), (512, 512));

    let mut dec = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&jpg));
    let zrgb = dec.decode().expect("zune decode");
    let mut worst = 0i32;
    for i in 0..zrgb.len() / 3 {
        for c in 0..3 {
            worst = worst.max((zrgb[i * 3 + c] as i32 - rgba[i * 4 + c] as i32).abs());
        }
    }
    assert!(worst <= 4, "gpu vs zune max diff {worst}");
}

/// Inject an APP1 EXIF segment (II TIFF, tag 0x0112) right after SOI.
fn with_exif_orient(jpg: &[u8], orient: u8) -> Vec<u8> {
    assert_eq!(&jpg[..2], &[0xFF, 0xD8]);
    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"Exif\0\0II");
    tiff.extend_from_slice(&42u16.to_le_bytes());
    tiff.extend_from_slice(&8u32.to_le_bytes());
    tiff.extend_from_slice(&1u16.to_le_bytes());
    tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // tag Orientation
    tiff.extend_from_slice(&3u16.to_le_bytes()); // type SHORT
    tiff.extend_from_slice(&1u32.to_le_bytes()); // count
    tiff.extend_from_slice(&(orient as u32).to_le_bytes());
    tiff.extend_from_slice(&0u32.to_le_bytes()); // next IFD
    let mut out = jpg[..2].to_vec();
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&((tiff.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(&tiff);
    out.extend_from_slice(&jpg[2..]);
    out
}

/// CPU-side reference transform for EXIF orientation value `o` applied
/// to a packed RGB buffer (what image::apply_orientation does).
fn orient_rgb(rgb: &[u8], w: usize, h: usize, o: u8) -> (Vec<u8>, usize, usize) {
    let at = |x: usize, y: usize| (y * w + x) * 3;
    let (ow, oh) = if o >= 5 { (h, w) } else { (w, h) };
    let mut out = vec![0u8; rgb.len()];
    for y in 0..oh {
        for x in 0..ow {
            let (sx, sy) = match o {
                2 => (w - 1 - x, y),
                3 => (w - 1 - x, h - 1 - y),
                4 => (x, h - 1 - y),
                5 => (y, x),
                6 => (y, h - 1 - x),
                7 => (w - 1 - y, h - 1 - x),
                8 => (w - 1 - y, x),
                _ => (x, y),
            };
            out[(y * ow + x) * 3..][..3].copy_from_slice(&rgb[at(sx, sy)..][..3]);
        }
    }
    (out, ow, oh)
}

#[test]
fn exif_orientation_applied_on_gpu() {
    let Some(jpg) = cjpeg(&gradient_ppm(300, 200), &["-quality", "92", "-sample", "1x1"]) else {
        eprintln!("cjpeg missing — skipping");
        return;
    };
    if !gpujpeg::available() {
        eprintln!("gpujpeg not compiled in — skipping");
        return;
    }
    let mut dec = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&jpg));
    let zrgb = dec.decode().expect("zune decode");

    for o in 1..=8u8 {
        let j2 = with_exif_orient(&jpg, o);
        let (rgba, w, h) = gpujpeg::decode_rgba(&j2).unwrap_or_else(|| panic!("o{o} decode"));
        let (want, ww, hh) = orient_rgb(&zrgb, 300, 200, o);
        assert_eq!((w as usize, h as usize), (ww, hh), "o{o} dims");
        let mut worst = 0i32;
        for i in 0..want.len() / 3 {
            for c in 0..3 {
                worst = worst.max((want[i * 3 + c] as i32 - rgba[i * 4 + c] as i32).abs());
            }
        }
        assert!(worst <= 4, "o{o} gpu vs oriented-zune max diff {worst}");
    }
}

#[test]
fn non_jpeg_falls_back() {
    assert_eq!(gpujpeg::decode_rgba(b"not a jpeg at all"), None);
    assert_eq!(gpujpeg::decode_rgba(&[]), None);
    assert!(gpujpeg::open(b"not a jpeg at all").is_none());
    assert!(gpujpeg::open(&[]).is_none());
}
