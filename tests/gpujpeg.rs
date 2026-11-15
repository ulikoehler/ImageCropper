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
    let Some(jpg) = cjpeg(&gradient_ppm(512, 512), &["-quality", "90", "-progressive"]) else {
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
    let Some(jpg) = cjpeg(
        &gradient_ppm(300, 200),
        &["-quality", "92", "-sample", "1x1"],
    ) else {
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

/// Rewrite component identifiers throughout a baseline JPEG: SOF spec
/// bytes and every SOS component selector. `f` maps old id -> new id
/// (None keeps the byte). Walks markers only up to the first SOS —
/// enough for the single-scan files the encoder emits.
fn patch_comp_ids(jpg: &[u8], f: impl Fn(u8) -> Option<u8>) -> (Vec<u8>, usize) {
    let mut out = jpg.to_vec();
    let mut patched = 0usize;
    let mut p = 2usize;
    while p + 4 <= out.len() && out[p] == 0xFF {
        let m = out[p + 1];
        if m == 0x00 || m == 0x01 || (0xD0..=0xD9).contains(&m) {
            p += 2;
            continue;
        }
        let len = u16::from_be_bytes([out[p + 2], out[p + 3]]) as usize;
        if p + 2 + len > out.len() {
            break;
        }
        match m {
            // SOFn (not DHT/JPG/DAC): comp specs at +10, 3 bytes each
            0xC0..=0xCF if m != 0xC4 && m != 0xC8 && m != 0xCC => {
                let n = out[p + 9] as usize;
                for k in 0..n.min(4) {
                    let o = p + 10 + k * 3;
                    if o < out.len() {
                        if let Some(v) = f(out[o]) {
                            out[o] = v;
                            patched += 1;
                        }
                    }
                }
            }
            0xDA => {
                let ns = out[p + 4] as usize;
                for k in 0..ns.min(4) {
                    let o = p + 5 + k * 2;
                    if o < out.len() {
                        if let Some(v) = f(out[o]) {
                            out[o] = v;
                            patched += 1;
                        }
                    }
                }
                break; // entropy data follows — stop marker walk
            }
            _ => {}
        }
        p += 2 + len;
    }
    (out, patched)
}

/// Regression: component IDs are arbitrary — real-world files (seen in
/// Lightroom exports) number them 0,1,2. The frame's unused comp slots
/// are Comp::default() with id==0, so the SOS->component lookup must
/// only search real components; otherwise id 0 binds to the phantom
/// 4th slot, its coefficients are never decoded (flat luma) and the
/// interleaved bitstream desyncs.
#[test]
fn component_id_zero_decodes() {
    if !gpujpeg::available() {
        eprintln!("gpujpeg not compiled in — skipping");
        return;
    }
    let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(96, 80, |x, y| {
        image::Rgb([(x * 3) as u8, (y * 5) as u8, ((x + y) % 251) as u8])
    }));
    let mut jpg = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut jpg), image::ImageFormat::Jpeg)
        .expect("encode jpeg");

    // ids 1,2,3 -> 0,1,2 (subtract one; keeps them unique and in order)
    let (jpg, patched) = patch_comp_ids(&jpg, |id| id.checked_sub(1));
    assert!(patched >= 6, "expected SOF+SOS id rewrites, got {patched}");

    // zune must still decode it — ids are arbitrary per spec
    let mut dec = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&jpg));
    dec.set_options(zune_jpeg::zune_core::options::DecoderOptions::default().set_strict_mode(false));
    let zrgb = dec.decode().expect("zune decode of id-0 jpeg");

    let (rgba, w, h) = gpujpeg::decode_rgba(&jpg).expect("gpu decode id-0 jpeg");
    assert_eq!((w, h), (96, 80));
    let mut worst = 0i32;
    let mut sum = 0i64;
    for i in 0..zrgb.len() / 3 {
        for c in 0..3 {
            let d = (zrgb[i * 3 + c] as i32 - rgba[i * 4 + c] as i32).abs();
            worst = worst.max(d);
            sum += d as i64;
        }
    }
    assert!(
        worst <= 4 && sum * 10 < zrgb.len() as i64,
        "gpu vs zune: worst {worst} mean {:.3}",
        sum as f64 / zrgb.len() as f64
    );
}

#[test]
fn non_jpeg_falls_back() {
    assert_eq!(gpujpeg::decode_rgba(b"not a jpeg at all"), None);
    assert_eq!(gpujpeg::decode_rgba(&[]), None);
    assert!(gpujpeg::open(b"not a jpeg at all").is_none());
    assert!(gpujpeg::open(&[]).is_none());
}

/// Smooth gradient — no wraparound steps, so lossy JPEG artifacts
/// (ringing, chroma bleed) stay within a few LSB.
fn gradient_rgba(w: usize, h: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            v.extend_from_slice(&[
                (x * 255 / w.max(1)) as u8,
                (y * 255 / h.max(1)) as u8,
                ((x + y) * 255 / (w + h).max(1)) as u8,
                255,
            ]);
        }
    }
    v
}

/// GPU-encode RGBA → JFIF, then verify it decodes (zune) to the same
/// dims with plausible pixels for a smooth gradient.
#[test]
fn encode_rgba_roundtrips() {
    if !gpujpeg::available() {
        eprintln!("gpujpeg not compiled in — skipping");
        return;
    }
    let (w, h) = (200usize, 130usize);
    let rgba = gradient_rgba(w, h);
    let jpg = gpujpeg::encode_rgba(&rgba, w as u32, h as u32, 90).expect("gpu encode");
    assert_eq!(&jpg[..2], &[0xFF, 0xD8]);
    assert_eq!(&jpg[jpg.len() - 2..], &[0xFF, 0xD9]);

    let mut dec = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&jpg));
    let rgb = dec.decode().expect("zune decode of gpu jpeg");
    assert_eq!(rgb.len(), w * h * 3);
    // Lossy but a smooth gradient: worst diff must stay small.
    let mut worst = 0i32;
    for i in 0..w * h {
        for c in 0..3 {
            worst = worst.max((rgb[i * 3 + c] as i32 - rgba[i * 4 + c] as i32).abs());
        }
    }
    assert!(worst <= 16, "encode roundtrip max diff {worst}");
}

/// Session encode_resized: GPU decode+resize+encode, dims fit, decodes.
#[test]
fn session_encode_resized_roundtrips() {
    if !gpujpeg::available() {
        eprintln!("gpujpeg not compiled in — skipping");
        return;
    }
    let Some(jpg) = cjpeg(&gradient_ppm(320, 240), &["-quality", "90"]) else {
        eprintln!("cjpeg missing — skipping");
        return;
    };
    let Some(s) = gpujpeg::open(&jpg) else {
        panic!("session open failed");
    };
    let (enc, w, h) = s.encode_resized(64, 64, 85).expect("encode_resized");
    assert_eq!((w, h), (64, 48));
    let mut dec = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&enc));
    let rgb = dec.decode().expect("zune decode");
    assert_eq!(rgb.len(), 64 * 48 * 3);

    // Full-res session encode too.
    let full = s.encode_jpeg(85).expect("encode_jpeg");
    let mut dec = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&full));
    let rgb = dec.decode().expect("zune decode full");
    assert_eq!(rgb.len(), 320 * 240 * 3);
}

/// Independent check: libjpeg's `djpeg` must decode our GPU-encoded
/// JFIF — not just zune — and produce sane pixels.
#[test]
fn encode_decodes_with_libjpeg() {
    if !gpujpeg::available() {
        eprintln!("gpujpeg not compiled in — skipping");
        return;
    }
    let (w, h) = (176usize, 144usize);
    let rgba = gradient_rgba(w, h);
    let jpg = gpujpeg::encode_rgba(&rgba, w as u32, h as u32, 92).expect("gpu encode");
    let tmp = std::env::temp_dir().join(format!("ic-gpuenc-{}.jpg", std::process::id()));
    std::fs::write(&tmp, &jpg).unwrap();
    let out = Command::new("djpeg")
        .arg("-ppm")
        .arg("-outfile")
        .arg("/dev/stdout")
        .arg(&tmp)
        .output();
    let _ = std::fs::remove_file(&tmp);
    let Ok(out) = out else {
        eprintln!("djpeg missing — skipping");
        return;
    };
    assert!(out.status.success(), "djpeg rejected gpu jpeg");
    // P6\n<w> <h>\n255\n then w*h*3 bytes
    let ppm = &out.stdout;
    let hdr = format!("P6\n{w} {h}\n255\n").into_bytes();
    assert!(ppm.starts_with(&hdr), "djpeg header mismatch");
    let px = &ppm[hdr.len()..];
    assert_eq!(px.len(), w * h * 3);
    let mut worst = 0i32;
    for i in 0..w * h {
        for c in 0..3 {
            worst = worst.max((px[i * 3 + c] as i32 - rgba[i * 4 + c] as i32).abs());
        }
    }
    assert!(worst <= 16, "djpeg roundtrip max diff {worst}");
}

/// Deferred finish: submit several encode chains back-to-back, finish
/// them afterwards — the pipelining pattern the preview pool uses.
#[test]
fn deferred_encode_jobs() {
    if !gpujpeg::available() {
        eprintln!("gpujpeg not compiled in — skipping");
        return;
    }
    let Some(jpg) = cjpeg(&gradient_ppm(256, 192), &["-quality", "90"]) else {
        eprintln!("cjpeg missing — skipping");
        return;
    };
    let Some(s) = gpujpeg::open(&jpg) else {
        panic!("session open failed");
    };
    let jobs: Vec<_> = [128u32, 64, 32]
        .iter()
        .map(|&d| s.encode_resized_start(d, d, 85).expect("start"))
        .collect();
    for (job, &d) in jobs.into_iter().zip([128u32, 64, 32].iter()) {
        let (enc, w, h) = job.finish().expect("finish");
        assert!(w <= d && h <= d);
        let mut dec = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&enc));
        let rgb = dec.decode().expect("zune decode");
        assert_eq!(rgb.len(), (w * h) as usize * 3);
    }
}

/// Arbitrary in-flight images: several threads decode+resize+encode
/// concurrently — every result must be individually correct. This is
/// the regression test for per-submission synchronization (no global
/// GPU lock, no wait-for-latest-submission).
#[test]
fn concurrent_inflight_images() {
    if !gpujpeg::available() {
        eprintln!("gpujpeg not compiled in — skipping");
        return;
    }
    let Some(jpg_a) = cjpeg(&gradient_ppm(256, 192), &["-quality", "90"]) else {
        eprintln!("cjpeg missing — skipping");
        return;
    };
    let Some(jpg_b) = cjpeg(&gradient_ppm(160, 200), &["-quality", "85", "-progressive"]) else {
        eprintln!("cjpeg missing — skipping");
        return;
    };
    let ja = std::sync::Arc::new(jpg_a);
    let jb = std::sync::Arc::new(jpg_b);

    let mut handles = Vec::new();
    for t in 0..8 {
        let (ja, jb) = (ja.clone(), jb.clone());
        handles.push(std::thread::spawn(move || {
            let jpg = if t % 2 == 0 { &ja } else { &jb };
            let (want_w, want_h) = if t % 2 == 0 { (256, 192) } else { (160, 200) };
            for i in 0..3 {
                let Some(s) = gpujpeg::open(jpg) else {
                    panic!("t{t}i{i}: open failed");
                };
                assert_eq!((s.width, s.height), (want_w, want_h), "t{t}i{i}");
                let (enc, w, h) = s.encode_resized(48, 48, 80).expect("encode_resized");
                assert_eq!(&enc[..2], &[0xFF, 0xD8]);
                let mut dec = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&enc));
                let rgb = dec.decode().expect("zune decode");
                assert_eq!(rgb.len(), (w * h) as usize * 3, "t{t}i{i}");
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
}

/// Rewrite the SOF width/height — fake huge dims without a huge image.
fn patch_sof_dims(jpg: &[u8], w: u16, h: u16) -> Vec<u8> {
    let mut out = jpg.to_vec();
    let mut p = 2usize;
    while p + 10 <= out.len() && out[p] == 0xFF {
        let m = out[p + 1];
        if m == 0x00 || m == 0x01 || (0xD0..=0xD9).contains(&m) {
            p += 2;
            continue;
        }
        if m == 0xDA {
            break;
        }
        let len = u16::from_be_bytes([out[p + 2], out[p + 3]]) as usize;
        if p + 2 + len > out.len() {
            break;
        }
        if (0xC0..=0xCF).contains(&m) && m != 0xC4 && m != 0xC8 && m != 0xCC {
            out[p + 5..p + 7].copy_from_slice(&h.to_be_bytes());
            out[p + 7..p + 9].copy_from_slice(&w.to_be_bytes());
            return out;
        }
        p += 2 + len;
    }
    panic!("no SOF marker found");
}

/// A JPEG whose GPU buffers would exceed the device limits must be
/// refused (None → CPU fallback), never panic inside create_buffer.
#[test]
fn oversized_jpeg_falls_back() {
    if !gpujpeg::available() {
        eprintln!("gpujpeg not compiled in — skipping");
        return;
    }
    let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(16, 16, |x, y| {
        image::Rgb([(x * 16) as u8, (y * 16) as u8, 128])
    }));
    let mut jpg = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut jpg), image::ImageFormat::Jpeg)
        .expect("encode jpeg");

    // 65535×65535 needs tens of GB of coefficient/RGBA buffers — far
    // past any adapter's max_buffer_size / binding limits.
    let jpg = patch_sof_dims(&jpg, u16::MAX, u16::MAX);
    let (w, h, _) = gpujpeg::probe(&jpg).expect("probe still parses");
    assert_eq!((w, h), (u16::MAX as u32, u16::MAX as u32));

    assert!(gpujpeg::open(&jpg).is_none(), "oversized decode must fall back");
    assert!(gpujpeg::decode_rgba(&jpg).is_none());
}
