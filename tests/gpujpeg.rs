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
fn dri0_falls_back() {
    let Some(jpg) = cjpeg(&gradient_ppm(64, 64), &["-quality", "90"]) else {
        eprintln!("cjpeg missing — skipping");
        return;
    };
    if gpujpeg::available() {
        // plain cjpeg output has no restart markers -> CPU path
        assert_eq!(gpujpeg::decode_rgba(&jpg), None);
    }
}

#[test]
fn non_jpeg_falls_back() {
    assert_eq!(gpujpeg::decode_rgba(b"not a jpeg at all"), None);
    assert_eq!(gpujpeg::decode_rgba(&[]), None);
}
