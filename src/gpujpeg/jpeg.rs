//! JFIF parser + GPU buffer packing — pure-Rust port of the host side
//! of native/gpujpegdec.cpp (which was itself ported from
//! VulkanExamples/p80_gpujpegdec). All values are bit-for-bit
//! identical to the C++ version.

/// Zigzag position -> natural (row-major) index inside an 8x8 block.
pub const ZIG: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

#[derive(Clone)]
pub struct HuffTable {
    pub counts: [u8; 16], // BITS: #codes of each length 1..16
    pub vals: Vec<u8>,    // HUFFVAL symbols in file order
    pub mincode: [i32; 16],
    pub maxcode: [i32; 16],
    pub valptr: [i32; 16],
    pub lut8: [u32; 256], // (len<<8)|sym, 0 = code longer than 8
}

impl Default for HuffTable {
    fn default() -> Self {
        HuffTable {
            counts: [0; 16],
            vals: Vec::new(),
            mincode: [0; 16],
            maxcode: [0; 16],
            valptr: [0; 16],
            lut8: [0; 256],
        }
    }
}

#[derive(Default, Clone, Copy)]
pub struct Comp {
    pub id: i32,
    pub hs: i32,
    pub vs: i32,
    pub tq: i32,
    pub td: i32,
    pub ta: i32,
    pub bw: i32,
    pub bh: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Clone, Copy)]
pub struct Interval {
    pub byte_start: u32,
    pub mcu_start: u32,
    pub n_mcu: u32,
}

pub struct Jpg<'a> {
    pub data: &'a [u8],
    pub w: i32,
    pub h: i32,
    pub comps: [Comp; 4],
    pub n_comp: i32,
    pub max_h: i32,
    pub max_v: i32,
    pub mcus_x: i32,
    pub mcus_y: i32,
    pub dri: i32,
    pub qt: [[u16; 64]; 4], // natural order
    pub dctab: [HuffTable; 4],
    pub actab: [HuffTable; 4],
    pub have_dht: bool,
    pub scan_off: u32,
    pub scan_len: u32,
    pub intervals: Vec<Interval>,
    pub total_mcus: i32,
}

impl<'a> Jpg<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Jpg {
            data,
            w: 0,
            h: 0,
            comps: [Comp::default(); 4],
            n_comp: 0,
            max_h: 1,
            max_v: 1,
            mcus_x: 0,
            mcus_y: 0,
            dri: 0,
            qt: [[0; 64]; 4],
            dctab: [
                HuffTable::default(),
                HuffTable::default(),
                HuffTable::default(),
                HuffTable::default(),
            ],
            actab: [
                HuffTable::default(),
                HuffTable::default(),
                HuffTable::default(),
                HuffTable::default(),
            ],
            have_dht: false,
            scan_off: 0,
            scan_len: 0,
            intervals: Vec::new(),
            total_mcus: 0,
        }
    }
}

pub fn build_canon(t: &mut HuffTable) {
    let mut code = 0i32;
    let mut k = 0i32;
    for l in 0..16 {
        if t.counts[l] > 0 {
            t.mincode[l] = code;
            t.valptr[l] = k - code; // index = valptr + code
            t.maxcode[l] = code + t.counts[l] as i32 - 1;
        } else {
            t.mincode[l] = 0;
            t.valptr[l] = k;
            t.maxcode[l] = -1;
        }
        code = (code + t.counts[l] as i32) << 1;
        k += t.counts[l] as i32;
    }
    // 8-bit prefix LUT: every len<=8 code covers 2^(8-len) suffixes.
    code = 0;
    k = 0;
    for l in 1..=8usize {
        for i in 0..t.counts[l - 1] as usize {
            let base = (code << (8 - l)) as usize;
            let span = 1usize << (8 - l);
            let e = ((l as u32) << 8) | t.vals[k as usize + i] as u32;
            for s in 0..span {
                t.lut8[base + s] = e;
            }
            code += 1;
        }
        code <<= 1;
        k += t.counts[l - 1] as i32;
    }
}

pub fn parse_jpg(j: &mut Jpg<'_>) -> bool {
    let d = &j.data;
    if d.len() < 4 || d[0] != 0xFF || d[1] != 0xD8 {
        return false;
    }
    let mut p = 2usize;
    let mut have_sof = false;
    'outer: while p + 1 < d.len() {
        // find next marker (FF followed by non-00)
        if d[p] != 0xFF {
            p += 1;
            continue;
        }
        let m = d[p + 1];
        if m == 0x00 {
            p += 2;
            continue;
        } // stuffed data byte
        if (0xD0..=0xD7).contains(&m) {
            p += 2;
            continue;
        } // stray RST
        if m == 0xD8 || m == 0x01 {
            p += 2;
            continue;
        } // SOI/TEM
        if m == 0xD9 {
            break;
        } // EOI
        if p + 3 >= d.len() {
            break;
        }
        let len = ((d[p + 2] as usize) << 8) | d[p + 3] as usize;
        let s_off = p + 4;
        let slen = len - 2;
        if p + 2 + len > d.len() {
            return false; // truncated
        }
        let s = &d[s_off..s_off + slen];

        match m {
            0xDB => {
                // DQT
                let mut o = 0usize;
                while o < slen {
                    let pq = s[o] >> 4;
                    let tq = (s[o] & 15) as usize;
                    o += 1;
                    if tq > 3 || pq > 1 || o + if pq > 0 { 128 } else { 64 } > slen {
                        return false;
                    }
                    for i in 0..64 {
                        let v = if pq > 0 {
                            let v = ((s[o] as u16) << 8) | s[o + 1] as u16;
                            o += 2;
                            v
                        } else {
                            let v = s[o] as u16;
                            o += 1;
                            v
                        };
                        j.qt[tq][ZIG[i] as usize] = v; // -> natural order
                    }
                }
            }
            0xC4 => {
                // DHT (possibly several tables per segment)
                let mut o = 0usize;
                while o < slen {
                    if o + 17 > slen {
                        return false;
                    }
                    let tc = s[o] >> 4;
                    let th = (s[o] & 15) as usize;
                    o += 1;
                    if th > 3 || tc > 1 {
                        return false;
                    }
                    let t = if tc > 0 {
                        &mut j.actab[th]
                    } else {
                        &mut j.dctab[th]
                    };
                    let mut n = 0usize;
                    for i in 0..16 {
                        t.counts[i] = s[o + i];
                        n += t.counts[i] as usize;
                    }
                    o += 16;
                    if o + n > slen || n > 256 {
                        return false;
                    }
                    t.vals = s[o..o + n].to_vec();
                    o += n;
                    j.have_dht = true;
                }
            }
            0xC0 | 0xC1 => {
                // SOF0 baseline / SOF1 ext sequential
                if s[0] != 8 {
                    return false;
                }
                j.h = ((s[1] as i32) << 8) | s[2] as i32;
                j.w = ((s[3] as i32) << 8) | s[4] as i32;
                j.n_comp = s[5] as i32;
                if j.n_comp != 1 && j.n_comp != 3 {
                    return false;
                }
                for i in 0..j.n_comp as usize {
                    let c = &mut j.comps[i];
                    c.id = s[6 + i * 3] as i32;
                    c.hs = (s[7 + i * 3] >> 4) as i32;
                    c.vs = (s[7 + i * 3] & 15) as i32;
                    c.tq = s[8 + i * 3] as i32;
                    j.max_h = j.max_h.max(c.hs);
                    j.max_v = j.max_v.max(c.vs);
                }
                have_sof = true;
            }
            0xC2 => {
                return false; // progressive JPEG unsupported
            }
            0xDD => {
                // DRI
                j.dri = ((s[0] as i32) << 8) | s[1] as i32;
            }
            0xDA => {
                // SOS — entropy data follows the header
                let ns = s[0] as i32;
                if ns != j.n_comp {
                    return false;
                }
                for i in 0..ns as usize {
                    let cs = s[1 + i * 2] as i32;
                    let mut ci = None;
                    for (k, cc) in j.comps.iter().enumerate() {
                        if cc.id == cs {
                            ci = Some(k);
                        }
                    }
                    let Some(ci) = ci else { return false };
                    j.comps[ci].td = (s[2 + i * 2] >> 4) as i32;
                    j.comps[ci].ta = (s[2 + i * 2] & 15) as i32;
                    if j.comps[ci].td > 3 || j.comps[ci].ta > 3 {
                        return false;
                    }
                }
                j.scan_off = (s_off + slen) as u32;
                // entropy data ends before the next unstuffed marker
                let mut q = j.scan_off as usize;
                while q + 1 < d.len() {
                    if d[q] == 0xFF && d[q + 1] != 0x00 && !(0xD0..=0xD7).contains(&d[q + 1]) {
                        break;
                    }
                    q += 1;
                }
                j.scan_len = (q - j.scan_off as usize) as u32;
                break 'outer;
            }
            _ => {}
        }
        p += 2 + len;
    }
    if !have_sof || !j.have_dht || j.scan_len == 0 {
        return false;
    }

    // ---- geometry ----
    j.mcus_x = (j.w + 8 * j.max_h - 1) / (8 * j.max_h);
    j.mcus_y = (j.h + 8 * j.max_v - 1) / (8 * j.max_v);
    j.total_mcus = j.mcus_x * j.mcus_y;
    for i in 0..j.n_comp as usize {
        let c = &mut j.comps[i];
        c.bw = j.mcus_x * c.hs;
        c.bh = j.mcus_y * c.vs;
        c.w = (j.w * c.hs + j.max_h - 1) / j.max_h;
        c.h = (j.h * c.vs + j.max_v - 1) / j.max_v;
    }

    // ---- restart intervals: scan for FFD0..FFD7 boundaries ----
    j.intervals.push(Interval {
        byte_start: 0,
        mcu_start: 0,
        n_mcu: 0,
    });
    let sd = &d[j.scan_off as usize..];
    let mut found = 0i64;
    let mut i = 0usize;
    while i + 1 < j.scan_len as usize {
        if sd[i] == 0xFF && (0xD0..=0xD7).contains(&sd[i + 1]) {
            found += 1;
            let last = j.intervals.last_mut().unwrap();
            last.n_mcu = (found * i64::from(j.dri.max(1))) as u32 - last.mcu_start;
            j.intervals.push(Interval {
                byte_start: (i + 2) as u32,
                mcu_start: (found as u32) * j.dri.max(1) as u32,
                n_mcu: 0,
            });
            i += 1;
        }
        i += 1;
    }
    if found == 0 {
        // no markers (DRI=0 or missing): one interval for everything
        j.intervals.truncate(1);
        j.intervals[0] = Interval {
            byte_start: 0,
            mcu_start: 0,
            n_mcu: j.total_mcus as u32,
        };
    } else {
        let last = j.intervals.last_mut().unwrap();
        last.n_mcu = j.total_mcus as u32 - last.mcu_start;
    }
    true
}

// ---- GPU buffer layouts ----
// Tables buffer words:
//   [0x000..0x0FF]  qtab: 4 x 64 natural-order quant values
//   per table t (8 total: 0-3 DC, 4-7 AC), base = 256 + t*816:
//     +0    mincode[16]   +16  maxcode[16]   +32  valptr[16]
//     +48   lut8[256]     +304 huffval[256]
pub const TAB_WORDS: usize = 256 + 8 * 816;
const Q_BASE: usize = 0;
const HUFF_BASE: usize = 256;

// Flat 16-bit "fused" Huffman LUT — one lookup yields the symbol AND,
// whenever huffLen+extLen fits in the 16-bit window, the already
// sign-extended coefficient value. Entry (u32):
//   bit31 set   -> fused: [4:0]=totalLen [8:5]=r [12:9]=s [28:13]=val+32768
//   bit31 clear -> plain: [4:0]=huffLen [15:8]=sym   (huffLen==0 -> invalid)
pub const FLUT_WORDS: usize = 8 * 65536;

fn extend_ref(v: i32, s: i32) -> i32 {
    if s > 0 && v < (1 << (s - 1)) {
        v - (1 << s) + 1
    } else {
        v
    }
}

pub fn pack_flut(j: &Jpg<'_>) -> Vec<u32> {
    let mut fl = vec![0u32; FLUT_WORDS];
    for t in 0..8usize {
        let h = if t < 4 { &j.dctab[t] } else { &j.actab[t - 4] };
        for x in 0..65536usize {
            // canonical walk for the first code in this prefix
            let mut len = 0i32;
            let mut sym = 0i32;
            for l in 1..=16usize {
                let code = (x >> (16 - l)) as i32;
                if h.maxcode[l - 1] >= 0 && code <= h.maxcode[l - 1] {
                    len = l as i32;
                    sym = h.vals[(h.valptr[l - 1] + code) as usize] as i32;
                    break;
                }
            }
            if len == 0 {
                continue;
            }
            let (mut r, mut s) = (0i32, sym & 15);
            if t >= 4 {
                r = sym >> 4;
            } else {
                s = sym; // DC: symbol is size
            }
            if len + s <= 16 {
                let ext = (x as i32 >> (16 - len - s)) & ((1 << s) - 1);
                let val = extend_ref(ext, s);
                fl[t * 65536 + x] = 0x80000000
                    | ((len + s) as u32)
                    | ((r as u32) << 5)
                    | ((s as u32) << 9)
                    | (((val + 32768) as u32) << 13);
            } else {
                fl[t * 65536 + x] = (len as u32) | ((sym as u32) << 8);
            }
        }
    }
    fl
}

pub fn pack_tables(j: &Jpg<'_>) -> Vec<i32> {
    let mut tab = vec![0i32; TAB_WORDS];
    for t in 0..4usize {
        for i in 0..64 {
            tab[Q_BASE + t * 64 + i] = j.qt[t][i] as i32;
        }
    }
    for t in 0..8usize {
        let h = if t < 4 { &j.dctab[t] } else { &j.actab[t - 4] };
        let b = HUFF_BASE + t * 816;
        for i in 0..16 {
            tab[b + i] = h.mincode[i];
            tab[b + 16 + i] = h.maxcode[i];
            tab[b + 32 + i] = h.valptr[i];
        }
        for i in 0..256usize {
            tab[b + 48 + i] = h.lut8[i] as i32;
            tab[b + 304 + i] = if i < h.vals.len() {
                h.vals[i] as i32
            } else {
                0
            };
        }
    }
    tab
}

// Per-component params, 16 int32 words each:
//   0 hs  1 vs  2 bw  3 bh  4 w  5 h  6 qt  7 dcTab  8 acTab
//   9 coefOfs(words)  10 planeOfs(bytes)
pub struct FrameGpu {
    pub comps: [[i32; 16]; 4],
    pub total_blocks: i32,
    pub plane_words: i32,
}

pub fn pack_comps(j: &Jpg<'_>) -> FrameGpu {
    let mut f = FrameGpu {
        comps: [[0; 16]; 4],
        total_blocks: 0,
        plane_words: 0,
    };
    let (mut coef_of, mut plane_of) = (0u32, 0u32);
    for i in 0..4 {
        let c = &j.comps[i];
        let cc = &mut f.comps[i];
        cc[0] = c.hs;
        cc[1] = c.vs;
        cc[2] = c.bw;
        cc[3] = c.bh;
        cc[4] = c.w;
        cc[5] = c.h;
        cc[6] = c.tq;
        cc[7] = c.td; // dctab[t] -> tabs[t]
        cc[8] = 4 + c.ta; // actab[t] -> tabs[4+t]
        cc[9] = coef_of as i32;
        cc[10] = plane_of as i32;
        if i < j.n_comp as usize {
            coef_of += (c.bw * c.bh) as u32 * 32; // 64 coefs / 2 per word
            plane_of += (c.bw * 8 * c.bh * 8) as u32; // plane bytes
        }
        f.total_blocks += c.bw * c.bh;
    }
    f.plane_words = (plane_of / 4) as i32;
    f
}
