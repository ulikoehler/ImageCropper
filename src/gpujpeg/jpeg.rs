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
    pub wib: i32, // width_in_blocks (non-interleaved scan grid)
    pub hib: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Clone, Copy)]
pub struct Interval {
    pub byte_start: u32, // relative to the scan's data_off
    pub mcu_start: u32,
    pub n_mcu: u32,
}

#[derive(Clone, Copy, Default)]
pub struct ScanComp {
    pub comp: i32,
    pub td: i32,
    pub ta: i32,
}

pub struct Scan {
    pub ns: i32,
    pub sc: [ScanComp; 4],
    pub ss: i32,
    pub se: i32,
    pub ah: i32,
    pub al: i32,
    pub dri: i32, // restart interval in effect for this scan
    pub data_off: u32,
    pub data_len: u32,
    pub intervals: Vec<Interval>,
    /// Host-surveyed bit-reader states for virtual (DRI=0) intervals:
    /// 12 u32 words per interval — pos, buf, cnt, end, pred[4], eobrun,
    /// pad, 2 reserved. Loaded by flag-2 chunks instead of chaining.
    pub surv_st: Vec<u32>,
    pub total_mcus: u32, // MCU count in this scan's units
    // Tables snapshot AS OF this scan — progressive files commonly
    // redefine DHT/DQT between scans (per-scan optimized tables).
    pub dctab: [HuffTable; 4],
    pub actab: [HuffTable; 4],
    pub qt: [[u16; 64]; 4],
}

impl Default for Scan {
    fn default() -> Self {
        Scan {
            ns: 0,
            sc: [ScanComp::default(); 4],
            ss: 0,
            se: 0,
            ah: 0,
            al: 0,
            dri: 0,
            data_off: 0,
            data_len: 0,
            intervals: Vec::new(),
            surv_st: Vec::new(),
            total_mcus: 0,
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
            qt: [[0; 64]; 4],
        }
    }
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
    pub dri: i32,           // current DRI while scanning headers
    pub qt: [[u16; 64]; 4], // natural order
    pub dctab: [HuffTable; 4],
    pub actab: [HuffTable; 4],
    pub have_dht: bool,
    pub scans: Vec<Scan>,
    pub total_mcus: i32,
    pub progressive: bool,
    /// EXIF orientation 1..8 from APP1/TIFF tag 0x0112 (1 = none).
    pub orient: i32,
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
            scans: Vec::new(),
            total_mcus: 0,
            progressive: false,
            orient: 1,
        }
    }

    /// Output dims after EXIF orientation (swapped for orient 5-8).
    pub fn out_dims(&self) -> (u32, u32) {
        if self.orient >= 5 {
            (self.h as u32, self.w as u32)
        } else {
            (self.w as u32, self.h as u32)
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

/// APP1 "Exif\0\0" payload -> TIFF orientation 1..8 (bounds-checked).
fn parse_exif_orient(s: &[u8]) -> i32 {
    if s.len() < 14 || !s.starts_with(b"Exif\0\0") {
        return 1;
    }
    let t = &s[6..];
    let le = t[0] == b'I';
    if !le && !(t[0] == b'M' && t[1] == b'M') {
        return 1;
    }
    let u16v = |o: usize| -> u32 {
        if le {
            t[o] as u32 | ((t[o + 1] as u32) << 8)
        } else {
            ((t[o] as u32) << 8) | t[o + 1] as u32
        }
    };
    let u32v = |o: usize| -> u32 {
        if le {
            t[o] as u32
                | ((t[o + 1] as u32) << 8)
                | ((t[o + 2] as u32) << 16)
                | ((t[o + 3] as u32) << 24)
        } else {
            ((t[o] as u32) << 24)
                | ((t[o + 1] as u32) << 16)
                | ((t[o + 2] as u32) << 8)
                | t[o + 3] as u32
        }
    };
    if u16v(2) != 42 {
        return 1;
    }
    let ifd = u32v(4) as usize;
    if ifd + 2 > t.len() {
        return 1;
    }
    let cnt = u16v(ifd) as usize;
    for i in 0..cnt {
        let e = ifd + 2 + i * 12;
        if e + 12 > t.len() {
            break;
        }
        if u16v(e) == 0x0112 && u16v(e + 2) == 3 {
            let v = u16v(e + 8) as i32;
            return if (1..=8).contains(&v) { v } else { 1 };
        }
    }
    1
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
            0xC0 | 0xC1 | 0xC2 => {
                // SOF0 baseline / SOF1 ext sequential / SOF2 progressive
                if s[0] != 8 {
                    return false;
                }
                j.progressive = m == 0xC2;
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
            0xE1 => {
                // APP1: EXIF orientation
                j.orient = parse_exif_orient(s);
            }
            0xDD => {
                // DRI — takes effect for all scans that follow
                j.dri = ((s[0] as i32) << 8) | s[1] as i32;
            }
            0xDA => {
                // SOS — entropy data follows the header
                if slen < 1 {
                    return false;
                }
                let ns = s[0] as i32;
                if ns < 1 || ns > 4 || ns > j.n_comp || slen < (1 + 2 * ns + 3) as usize {
                    return false;
                }
                let mut sc = Scan {
                    ns,
                    sc: [ScanComp::default(); 4],
                    ss: 0,
                    se: 63,
                    ah: 0,
                    al: 0,
                    dri: j.dri,
                    data_off: 0,
                    data_len: 0,
                    intervals: Vec::new(),
                    surv_st: Vec::new(),
                    total_mcus: 0,
                    dctab: j.dctab.clone(),
                    actab: j.actab.clone(),
                    qt: j.qt,
                };
                for i in 0..ns as usize {
                    let cs = s[1 + i * 2] as i32;
                    let mut ci = None;
                    for (k, cc) in j.comps.iter().enumerate() {
                        if cc.id == cs {
                            ci = Some(k);
                        }
                    }
                    let Some(ci) = ci else { return false };
                    sc.sc[i].comp = ci as i32;
                    sc.sc[i].td = (s[2 + i * 2] >> 4) as i32;
                    sc.sc[i].ta = (s[2 + i * 2] & 15) as i32;
                    if sc.sc[i].td > 3 || sc.sc[i].ta > 3 {
                        return false;
                    }
                }
                sc.ss = s[1 + ns as usize * 2] as i32;
                sc.se = s[2 + ns as usize * 2] as i32;
                sc.ah = (s[3 + ns as usize * 2] >> 4) as i32;
                sc.al = (s[3 + ns as usize * 2] & 15) as i32;
                sc.data_off = (s_off + slen) as u32;
                // entropy data ends before the next unstuffed marker
                let mut q = sc.data_off as usize;
                while q + 1 < d.len() {
                    if d[q] == 0xFF && d[q + 1] != 0x00 && !(0xD0..=0xD7).contains(&d[q + 1]) {
                        break;
                    }
                    q += 1;
                }
                sc.data_len = (q - sc.data_off as usize) as u32;
                j.scans.push(sc);
                p = q;
                continue 'outer;
            }
            _ => {}
        }
        p += 2 + len;
    }
    if !have_sof || !j.have_dht || j.scans.is_empty() {
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
        // jdinput.c: width_in_blocks = ceil(W*hs/(maxH*8)) — this can
        // differ from bw (MCU-aligned); non-interleaved scans iterate
        // this grid.
        c.wib = (j.w * c.hs + j.max_h * 8 - 1) / (j.max_h * 8);
        c.hib = (j.h * c.vs + j.max_v * 8 - 1) / (j.max_v * 8);
    }

    // ---- per-scan restart intervals (FFD0..FFD7 boundaries) ----
    for sc in j.scans.iter_mut() {
        let total = if sc.ns == 1 {
            (j.comps[sc.sc[0].comp as usize].wib * j.comps[sc.sc[0].comp as usize].hib) as u32
        } else {
            j.total_mcus as u32
        };
        sc.total_mcus = total;
        sc.intervals.push(Interval {
            byte_start: 0,
            mcu_start: 0,
            n_mcu: 0,
        });
        let sd = &d[sc.data_off as usize..];
        let mut found = 0i64;
        let mut i = 0usize;
        while i + 1 < sc.data_len as usize {
            if sd[i] == 0xFF && (0xD0..=0xD7).contains(&sd[i + 1]) {
                found += 1;
                let last = sc.intervals.last_mut().unwrap();
                last.n_mcu = (found * i64::from(sc.dri.max(1))) as u32 - last.mcu_start;
                sc.intervals.push(Interval {
                    byte_start: (i + 2) as u32,
                    mcu_start: (found as u32) * sc.dri.max(1) as u32,
                    n_mcu: 0,
                });
                i += 1;
            }
            i += 1;
        }
        if found == 0 {
            // no markers (DRI=0 or missing): one interval for everything
            sc.intervals.truncate(1);
            sc.intervals[0] = Interval {
                byte_start: 0,
                mcu_start: 0,
                n_mcu: total,
            };
        } else {
            let last = sc.intervals.last_mut().unwrap();
            last.n_mcu = total - last.mcu_start;
        }
    }
    true
}

/// Scan -> entropy mode. AC bands are always non-interleaved (ns==1);
/// DC bands may be interleaved or per-component.
pub fn scan_mode(sc: &Scan) -> u32 {
    if sc.ss == 0 && sc.se == 63 && sc.ah == 0 && sc.al == 0 {
        0 // sequential full band
    } else if sc.ss == 0 {
        if sc.ah == 0 {
            1
        } else {
            2
        }
    } else if sc.ah == 0 {
        3
    } else {
        4
    }
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

fn same_table(a: &HuffTable, b: &HuffTable) -> bool {
    a.counts == b.counts && a.vals == b.vals
}

/// Build one scan's fused-LUT region. `prev` dedups unchanged tables:
/// when a table matches the previous scan's, its region is copied from
/// `prev_fl` (which the caller must pass along) instead of rebuilt.
pub fn pack_flut(sc: &Scan, prev: Option<(&Scan, &[u32])>) -> Vec<u32> {
    let mut fl = vec![0u32; FLUT_WORDS];
    for t in 0..8usize {
        let h = if t < 4 {
            &sc.dctab[t]
        } else {
            &sc.actab[t - 4]
        };
        if let Some((p, pfl)) = prev {
            let ph = if t < 4 { &p.dctab[t] } else { &p.actab[t - 4] };
            if same_table(h, ph) {
                fl[t * 65536..(t + 1) * 65536].copy_from_slice(&pfl[t * 65536..(t + 1) * 65536]);
                continue;
            }
        }
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

pub fn pack_tables(sc: &Scan) -> Vec<i32> {
    let mut tab = vec![0i32; TAB_WORDS];
    for t in 0..4usize {
        for i in 0..64 {
            tab[Q_BASE + t * 64 + i] = sc.qt[t][i] as i32;
        }
    }
    for t in 0..8usize {
        let h = if t < 4 {
            &sc.dctab[t]
        } else {
            &sc.actab[t - 4]
        };
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
//   9 coefOfs(words)  10 planeOfs(bytes)  11 wib  12 hib  13..15 pad
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
        cc[11] = c.wib;
        cc[12] = c.hib;
        if i < j.n_comp as usize {
            coef_of += (c.bw * c.bh) as u32 * 32; // 64 coefs / 2 per word
            plane_of += (c.bw * 8 * c.bh * 8) as u32; // plane bytes
        }
        f.total_blocks += c.bw * c.bh;
    }
    f.plane_words = (plane_of / 4) as i32;
    f
}

// ============================================================
//  Survey pass: virtual restart intervals for DRI=0 scans.
//
//  A serial Huffman-symbol walk over the entropy stream that mirrors
//  the GPU kernel's consumption exactly — but only tracks decoder
//  state (bit position, DC predictors, EOBRUN, pad), never pixels.
//  Snapshots every K_SURVEY_IV MCUs become virtual restart intervals
//  (flag-2 chunks): preloaded state turns the serial stream back into
//  independent parallel work. AC-refine scans additionally need the
//  coef buffer from prior scans — their survey is deferred until those
//  scans' GPU dispatch completed (host reads coefBuf).
// ============================================================
pub const K_SURVEY_IV: u32 = 256;

/// CPU mirror of the entropy shader's bit reader (identical semantics,
/// including marker-prefetch `pad` tracking).
struct SBit<'a> {
    d: &'a [u8],
    pos: u32,
    buf: u32,
    cnt: i32,
    end: bool,
    pad: i32,
}

impl<'a> SBit<'a> {
    fn rd_byte(&mut self) -> u32 {
        if self.end {
            return 0xFF;
        }
        let v = self.d[self.pos as usize] as u32;
        if v != 0xFF {
            self.pos += 1;
            return v;
        }
        let mut p = self.pos + 1;
        let mut n = 0xFFu32;
        while n == 0xFF && (p as usize) < self.d.len() {
            n = self.d[p as usize] as u32;
            if n == 0xFF {
                p += 1;
            }
        }
        if n == 0 {
            self.pos = p + 1;
            return 0xFF;
        }
        self.end = true;
        0xFF
    }
    fn fill(&mut self) {
        while self.cnt <= 24 {
            let v = self.rd_byte();
            self.buf = (self.buf << 8) | v;
            self.cnt += 8;
            if self.end {
                self.pad += 8;
            }
        }
    }
    fn peek_n(&mut self, n: u32) -> u32 {
        self.fill();
        if self.cnt as u32 >= n {
            return (self.buf >> (self.cnt as u32 - n)) & ((1 << n) - 1);
        }
        let real = self.cnt as u32;
        let v = (self.buf & ((1 << real) - 1)) << (n - real);
        v | ((1 << (n - real)) - 1)
    }
    fn eat(&mut self, n: u32) {
        self.cnt = if self.cnt as u32 >= n {
            self.cnt - n as i32
        } else {
            0
        };
        if self.pad > self.cnt {
            self.pad = self.cnt;
        }
        self.buf &= if self.cnt >= 32 {
            0xFFFF_FFFF
        } else {
            (1u32 << (self.cnt as u32)) - 1
        };
    }
    fn get1(&mut self) -> u32 {
        let v = self.peek_n(1);
        self.eat(1);
        v
    }
    fn rext(&mut self, s: i32) -> i32 {
        if s == 0 {
            return 0;
        }
        let i = self.peek_n(s as u32) as i32;
        self.eat(s as u32);
        if i < (1 << (s - 1)) {
            i - (1 << s) + 1
        } else {
            i
        }
    }
}

/// Canonical Huffman decode (mincode/maxcode/valptr walk).
fn hdec(b: &mut SBit<'_>, t: &HuffTable) -> i32 {
    let mut code = 0i32;
    for l in 0..16usize {
        code = (code << 1) | b.get1() as i32;
        if t.maxcode[l] >= 0 && code <= t.maxcode[l] {
            return t.vals[(t.valptr[l] + code) as usize] as i32;
        }
    }
    -1
}

/// Coef access for AC-refine surveys — same zigzag-pair packing as the
/// GPU coef buffer. The null impl returns 0 (used for non-refine scans).
pub trait SurvCoef {
    fn get(&self, outp: u32, k: i32) -> i32;
}
pub struct NoCoef;
impl SurvCoef for NoCoef {
    fn get(&self, _o: u32, _k: i32) -> i32 {
        0
    }
}
impl SurvCoef for &[u32] {
    fn get(&self, outp: u32, k: i32) -> i32 {
        let w2 = self[(outp + (k >> 1) as u32) as usize];
        let v = ((w2 >> ((k & 1) * 16)) & 0xFFFF) as i32;
        if v >= 32768 {
            v - 65536
        } else {
            v
        }
    }
}

/// One scan's entropy stream -> virtual interval table + per-interval
/// preloaded state (12 u32 words each). Mirrors the shader's per-MCU
/// symbol walk for all five modes; mode 4 reads coefs (nonzero walk).
/// Tables must already be canonicalized (build_canon).
/// Returns false on unrecoverable stream corruption.
pub fn survey_scan(j: &Jpg<'_>, sc: &mut Scan, coef: &dyn SurvCoef, fg: &FrameGpu) -> bool {
    let mode = scan_mode(sc);
    let mut b = SBit {
        d: j.data,
        pos: sc.data_off,
        buf: 0,
        cnt: 0,
        end: false,
        pad: 0,
    };
    let mut pred = [0i32; 4];
    let mut eobrun = 0u32;
    sc.intervals.clear();
    sc.surv_st.clear();
    let total = sc.total_mcus;

    macro_rules! snap {
        ($mcu_start:expr) => {{
            let mcu_start = $mcu_start;
            if let Some(last) = sc.intervals.last_mut() {
                last.n_mcu = mcu_start - last.mcu_start;
            }
            sc.intervals.push(Interval {
                byte_start: b.pos - sc.data_off,
                mcu_start,
                n_mcu: 0,
            });
            sc.surv_st.extend_from_slice(&[
                b.pos,
                b.buf,
                b.cnt as u32,
                u32::from(b.end),
                pred[0] as u32,
                pred[1] as u32,
                pred[2] as u32,
                pred[3] as u32,
                eobrun,
                b.pad as u32,
                0,
                0,
            ]);
        }};
    }

    for mcu in 0..total {
        if mcu % K_SURVEY_IV == 0 {
            snap!(mcu);
        }
        if b.end && b.cnt <= b.pad {
            continue; // nothing but marker padding — mirrors shader skip
        }
        if sc.ns > 1 {
            let mx = (mcu as i32) % j.mcus_x;
            let my = (mcu as i32) / j.mcus_x;
            for ci in 0..sc.ns as usize {
                let c = &j.comps[sc.sc[ci].comp as usize];
                let dt = &sc.dctab[sc.sc[ci].td as usize];
                let at = &sc.actab[sc.sc[ci].ta as usize];
                let cofs = fg.comps[sc.sc[ci].comp as usize][9] as u32;
                for v in 0..c.vs {
                    for h in 0..c.hs {
                        let bi = (my * c.vs + v) * c.bw + mx * c.hs + h;
                        let outp = cofs + bi as u32 * 32;
                        let _ = outp;
                        match mode {
                            0 => {
                                let s = hdec(&mut b, dt);
                                if s < 0 {
                                    return false;
                                }
                                pred[ci] += b.rext(s);
                                let mut k = 1;
                                while k < 64 {
                                    let sym = hdec(&mut b, at);
                                    if sym < 0 {
                                        return false;
                                    }
                                    let r = sym >> 4;
                                    let s2 = sym & 15;
                                    if s2 == 0 {
                                        if r == 15 {
                                            k += 16;
                                            continue;
                                        }
                                        break;
                                    }
                                    k += r;
                                    b.rext(s2); // consumed even OOB
                                    k += 1;
                                }
                            }
                            1 => {
                                let s = hdec(&mut b, dt);
                                if s < 0 {
                                    return false;
                                }
                                pred[ci] += b.rext(s);
                            }
                            2 => {
                                b.get1();
                            }
                            _ => {}
                        }
                    }
                }
            }
        } else {
            let comp = sc.sc[0].comp as usize;
            let dt = &sc.dctab[sc.sc[0].td as usize];
            let at = &sc.actab[sc.sc[0].ta as usize];
            let wib1 = j.comps[comp].wib;
            let bw = j.comps[comp].bw;
            let cofs = fg.comps[comp][9] as u32;
            let bx = (mcu as i32) % wib1;
            let by = (mcu as i32) / wib1;
            let outp = cofs + (by * bw + bx) as u32 * 32;
            match mode {
                0 | 1 => {
                    let s = hdec(&mut b, dt);
                    if s < 0 {
                        return false;
                    }
                    pred[0] += b.rext(s);
                    if mode == 0 {
                        let mut k = 1;
                        while k < 64 {
                            let sym = hdec(&mut b, at);
                            if sym < 0 {
                                return false;
                            }
                            let r = sym >> 4;
                            let s2 = sym & 15;
                            if s2 == 0 {
                                if r == 15 {
                                    k += 16;
                                    continue;
                                }
                                break;
                            }
                            k += r;
                            b.rext(s2); // consumed even OOB
                            k += 1;
                        }
                    }
                }
                2 => {
                    b.get1();
                }
                3 => {
                    if eobrun > 0 {
                        eobrun -= 1;
                    } else {
                        let mut k = sc.ss;
                        while k <= sc.se {
                            let sym = hdec(&mut b, at);
                            if sym < 0 {
                                return false;
                            }
                            let r = sym >> 4;
                            let s2 = sym & 15;
                            if s2 != 0 {
                                k += r;
                                b.rext(s2); // consumed even OOB
                                k += 1;
                            } else if r == 15 {
                                k += 16;
                            } else {
                                eobrun = 1u32 << r;
                                if r > 0 {
                                    eobrun += b.peek_n(r as u32);
                                    b.eat(r as u32);
                                }
                                eobrun -= 1;
                                break;
                            }
                        }
                    }
                }
                _ => {
                    // mode 4 — AC refinement: coef-dependent walk
                    let mut k = sc.ss;
                    if eobrun == 0 {
                        while k <= sc.se {
                            let sym = hdec(&mut b, at);
                            if sym < 0 {
                                return false;
                            }
                            let mut r = sym >> 4;
                            let s2 = sym & 15;
                            if s2 == 0 && r != 15 {
                                eobrun = 1u32 << r;
                                if r > 0 {
                                    eobrun += b.peek_n(r as u32);
                                    b.eat(r as u32);
                                }
                                break;
                            }
                            if s2 != 0 {
                                b.get1(); // new-coef sign bit
                            }
                            loop {
                                let v = coef.get(outp, k);
                                if v != 0 {
                                    b.get1(); // correction bit
                                } else {
                                    r -= 1;
                                    if r < 0 {
                                        break;
                                    }
                                }
                                k += 1;
                                if k > sc.se {
                                    break;
                                }
                            }
                            k += 1;
                        }
                    }
                    if eobrun > 0 {
                        while k <= sc.se {
                            if coef.get(outp, k) != 0 {
                                b.get1();
                            }
                            k += 1;
                        }
                        eobrun -= 1;
                    }
                }
            }
        }
    }
    if let Some(last) = sc.intervals.last_mut() {
        last.n_mcu = total - last.mcu_start;
    }
    true
}
