//! Host side of the GPU JPEG encoder: Annex-K tables, quality scaling,
//! canonical Huffman code tables, and JFIF container assembly.
//! The GPU writes per-MCU Huffman segments (see shaders::ENCODE); this
//! wraps them in markers — memcpy, no pixel work.

/// Bytes reserved per MCU segment in the output buffer. Worst case for
/// 4:2:0 is ~530B; 1024 leaves generous headroom (as the GLSL encoder).
pub const SEG_BYTES: usize = 1024;
pub const SEG_WORDS: u32 = (SEG_BYTES / 4) as u32;

const STD_LUMA_Q: [u8; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, //
    14, 13, 16, 24, 40, 57, 69, 56, 14, 17, 22, 29, 51, 87, 80, 62, //
    18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113, 92, //
    49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];
const STD_CHROMA_Q: [u8; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, //
    24, 26, 56, 99, 99, 99, 99, 99, 47, 66, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, //
    12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, //
    28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, //
    37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, //
    54, 47, 55, 62, 63,
];

const DC_LUMA_BITS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const DC_LUMA_VALS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const DC_CHROMA_BITS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const DC_CHROMA_VALS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const AC_LUMA_BITS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
const AC_LUMA_VALS: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, //
    0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, //
    0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0, 0x24, //
    0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, //
    0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, //
    0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, //
    0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, //
    0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, //
    0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, //
    0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, //
    0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, //
    0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, //
    0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, //
    0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, //
    0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
];
const AC_CHROMA_BITS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
const AC_CHROMA_VALS: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, //
    0x41, 0x51, 0x07, 0x61, 0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, //
    0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0, 0x15, //
    0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, //
    0x18, 0x19, 0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, //
    0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, //
    0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, //
    0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, //
    0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, //
    0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, //
    0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, //
    0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, //
    0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, //
    0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, //
    0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
];

/// JPEG quality-scaled quant table (standard formula, clamped 1..255).
fn scale_quant(base: &[u8; 64], quality: i32, out: &mut [u32]) {
    let quality = quality.clamp(1, 100);
    let scale = if quality < 50 {
        5000 / quality
    } else {
        200 - quality * 2
    };
    for i in 0..64 {
        out[i] = ((base[i] as i32 * scale + 50) / 100).clamp(1, 255) as u32;
    }
}

/// Canonical Huffman codes from bits+vals → out[symbol] = len<<16|code.
fn build_huff(bits: &[u8; 16], vals: &[u8], out: &mut [u32]) {
    let mut code = 0u32;
    let mut k = 0usize;
    for len in 1..=16usize {
        for _ in 0..bits[len - 1] {
            if k >= vals.len() {
                break;
            }
            out[vals[k] as usize] = ((len as u32) << 16) | code;
            k += 1;
            code += 1;
        }
        code <<= 1;
    }
}

/// Tab buffer layout for the ENCODE shader:
/// [0..64) qY, [64..128) qC, [128..144) dcY codes,
/// [144..400) acY, [400..416) dcC, [416..672) acC — 672 u32 words.
pub fn pack_enc_tables(quality: i32) -> Vec<u32> {
    let mut t = vec![0u32; 672];
    scale_quant(&STD_LUMA_Q, quality, &mut t[0..64]);
    scale_quant(&STD_CHROMA_Q, quality, &mut t[64..128]);
    build_huff(&DC_LUMA_BITS, &DC_LUMA_VALS, &mut t[128..144]);
    build_huff(&AC_LUMA_BITS, &AC_LUMA_VALS, &mut t[144..400]);
    build_huff(&DC_CHROMA_BITS, &DC_CHROMA_VALS, &mut t[400..416]);
    build_huff(&AC_CHROMA_BITS, &AC_CHROMA_VALS, &mut t[416..672]);
    t
}

fn u16be(v: &mut Vec<u8>, w: u32) {
    v.push((w >> 8) as u8);
    v.push((w & 0xFF) as u8);
}
fn marker(v: &mut Vec<u8>, m: u8) {
    v.push(0xFF);
    v.push(m);
}
fn dqt_seg(v: &mut Vec<u8>, id: u8, q: &[u32]) {
    marker(v, 0xDB);
    u16be(v, 2 + 1 + 64);
    v.push(id);
    for i in 0..64 {
        v.push(q[ZIGZAG[i]] as u8);
    }
}
fn dht_seg(v: &mut Vec<u8>, tcth: u8, bits: &[u8; 16], vals: &[u8]) {
    marker(v, 0xC4);
    u16be(v, (2 + 1 + 16 + vals.len()) as u32);
    v.push(tcth);
    v.extend_from_slice(bits);
    v.extend_from_slice(vals);
}

fn build_header(w: u32, h: u32, gray: bool, q_y: &[u32], q_c: &[u32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(1024);
    marker(&mut v, 0xD8); // SOI
    marker(&mut v, 0xE0); // APP0 JFIF
    u16be(&mut v, 16);
    v.extend_from_slice(b"JFIF\0");
    v.push(1);
    v.push(1);
    v.push(0);
    u16be(&mut v, 1);
    u16be(&mut v, 1);
    v.push(0);
    v.push(0);
    dqt_seg(&mut v, 0, q_y);
    if !gray {
        dqt_seg(&mut v, 1, q_c);
    }
    marker(&mut v, 0xC0); // SOF0
    u16be(&mut v, 8 + 3 * if gray { 1 } else { 3 });
    v.push(8);
    u16be(&mut v, h);
    u16be(&mut v, w);
    if gray {
        v.extend_from_slice(&[1, 1, 0x11, 0]);
    } else {
        v.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
    }
    dht_seg(&mut v, 0x00, &DC_LUMA_BITS, &DC_LUMA_VALS);
    dht_seg(&mut v, 0x10, &AC_LUMA_BITS, &AC_LUMA_VALS);
    if !gray {
        dht_seg(&mut v, 0x01, &DC_CHROMA_BITS, &DC_CHROMA_VALS);
        dht_seg(&mut v, 0x11, &AC_CHROMA_BITS, &AC_CHROMA_VALS);
    }
    marker(&mut v, 0xDD); // DRI
    u16be(&mut v, 4);
    u16be(&mut v, 1); // restart every MCU
    marker(&mut v, 0xDA); // SOS
    u16be(&mut v, 6 + 2 * if gray { 1 } else { 3 });
    if gray {
        v.extend_from_slice(&[1, 1, 0x00]);
    } else {
        v.extend_from_slice(&[3, 1, 0x00, 2, 0x11, 3, 0x11]);
    }
    v.extend_from_slice(&[0, 63, 0]); // Ss/Se/AhAl
    v
}

/// Assemble a baseline JFIF from the ENCODE output buffer read back as
/// LE bytes: `total` u32 sizes then `total` segments of SEG_BYTES each.
pub fn assemble_jpeg(
    raw: &[u8],
    w: u32,
    h: u32,
    mcus_x: u32,
    mcus_y: u32,
    gray: bool,
    tab: &[u32],
) -> Vec<u8> {
    let total = (mcus_x * mcus_y) as usize;
    let header = build_header(w, h, gray, &tab[0..64], &tab[64..128]);
    let size_at = |i: usize| u32::from_le_bytes(raw[i * 4..i * 4 + 4].try_into().unwrap()) as usize;
    let segs = &raw[total * 4..];

    let mut bytes = header.len() + 2;
    for i in 0..total {
        bytes += size_at(i).min(SEG_BYTES) + if i + 1 < total { 2 } else { 0 };
    }
    let mut jpg = Vec::with_capacity(bytes);
    jpg.extend_from_slice(&header);
    for i in 0..total {
        let n = size_at(i).min(SEG_BYTES);
        jpg.extend_from_slice(&segs[i * SEG_BYTES..i * SEG_BYTES + n]);
        if i + 1 < total {
            // RSTm between restart intervals (DRI=1)
            jpg.push(0xFF);
            jpg.push(0xD0 + (i % 8) as u8);
        }
    }
    jpg.push(0xFF);
    jpg.push(0xD9); // EOI
    jpg
}
