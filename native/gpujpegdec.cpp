// SPDX-License-Identifier: CC0-1.0
//
// Baseline JPEG DECODING in Vulkan compute shaders — the mirror image
// of p51_gpujpeg's encoder. Three dispatches:
//
//   1. entropy:  one invocation per restart interval decodes Huffman-
//      coded MCUs -> quantized DCT coefficients (zigzag order, i16
//      packed pairs). Restart markers (DRI>0) are what make this
//      parallel: each interval's bitstream and DC predictors are
//      independent. Files without restart markers decode as a single
//      interval — correct but serial (see README).
//   2. recon:    one invocation per 8x8 block: dequantize + libjpeg
//      "islow" integer IDCT -> per-component u8 sample planes.
//   3. upsample: one invocation per output pixel: libjpeg "fancy"
//      triangle upsampling + libjpeg fixed-point YCbCr->RGB -> RGBA8.
//
// The host only parses JFIF headers (DQT/DHT/SOF/DRI), builds the
// canonical Huffman tables and finds the restart-marker offsets —
// all the entropy bit-work and all the pixel math runs on the GPU.
//
//   ./app in.jpg out.ppm           decode entirely on the GPU
//   ./app --cpu in.jpg out.ppm     libjpeg reference (for comparison)
//   ./app --verify in.jpg          GPU decode + per-stage diff report
//   ./app --bench N in.jpg         N decodes, prints MP/s
//
// Build:  g++ -std=c++23 -O2 main.cpp -o app -lvulkan -lshaderc -ljpeg
// (no GLFW needed — compute only. PIL/libjpeg used by check.py.)

#include "vkmini.hpp"

#include <chrono>
#include <cmath>
#include <map>

// Zigzag position -> natural (row-major) index inside an 8x8 block.
static constexpr uint8_t kZig[64] = {
    0,  1,  8, 16,  9,  2,  3, 10,
    17, 24, 32, 25, 18, 11,  4,  5,
    12, 19, 26, 33, 40, 48, 41, 34,
    27, 20, 13,  6,  7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36,
    29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46,
    53, 60, 61, 54, 47, 55, 62, 63};

// ============================================================
//  JFIF parser — enough structure to drive the GPU stages.
// ============================================================

struct HuffTable {
    uint8_t counts[16]{};        // BITS: #codes of each length 1..16
    std::vector<uint8_t> vals;   // HUFFVAL symbols in file order
    // canonical decode tables (host-built)
    int32_t mincode[16]{}, maxcode[16]{}, valptr[16]{};
    uint32_t lut8[256]{};        // (len<<8)|sym, 0 = code longer than 8
};

struct Comp {
    int id = 0, hs = 1, vs = 1, tq = 0;
    int td = 0, ta = 0;          // scan table selectors (SOS)
    int bw = 0, bh = 0;          // block-grid dims (mcu-aligned)
    int w = 0, h = 0;            // downsampled sample dims
};

struct Interval { uint32_t byteStart, mcuStart, nMcu; };

struct Jpg {
    std::vector<uint8_t> data;
    int W = 0, H = 0;
    Comp comps[4];
    int nComp = 0;
    int maxH = 1, maxV = 1;
    int mcusX = 0, mcusY = 0;
    int dri = 0;                 // restart interval in MCUs (0 = none)
    uint16_t qt[4][64]{};        // natural order
    HuffTable dctab[4], actab[4];
    bool haveDht = false;
    // entropy-coded segment
    uint32_t scanOff = 0, scanLen = 0;
    std::vector<Interval> intervals;
    int totalMcus = 0;
};

static void buildCanon(HuffTable& t) {
    int code = 0, k = 0;
    for (int l = 0; l < 16; ++l) {
        if (t.counts[l]) {
            t.mincode[l] = code;
            t.valptr[l]  = k - code;          // index = valptr + code
            t.maxcode[l] = code + t.counts[l] - 1;
        } else {
            t.mincode[l] = 0; t.valptr[l] = k; t.maxcode[l] = -1;
        }
        code = (code + t.counts[l]) << 1;
        k += t.counts[l];
    }
    // 8-bit prefix LUT: every len<=8 code covers 2^(8-len) suffixes.
    code = 0; k = 0;
    for (int l = 1; l <= 8; ++l) {
        for (int i = 0; i < t.counts[l - 1]; ++i) {
            int base = code << (8 - l), span = 1 << (8 - l);
            uint32_t e = (uint32_t(l) << 8) | t.vals[k + i];
            for (int s = 0; s < span; ++s) t.lut8[base + s] = e;
            code++;
        }
        code <<= 1;
        k += t.counts[l - 1];
    }
}

static bool parseJpg(Jpg& j) {
    const auto& d = j.data;
    if (d.size() < 4 || d[0] != 0xFF || d[1] != 0xD8) {
        fprintf(stderr, "not a JPEG (no SOI)\n");
        return false;
    }
    size_t p = 2;
    bool haveSof = false;
    while (p + 1 < d.size()) {
        // find next marker (FF followed by non-00)
        if (d[p] != 0xFF) { p++; continue; }
        uint8_t m = d[p + 1];
        if (m == 0x00) { p += 2; continue; }   // stuffed data byte
        if (m >= 0xD0 && m <= 0xD7) { p += 2; continue; } // stray RST
        if (m == 0xD8 || m == 0x01) { p += 2; continue; } // SOI/TEM
        if (m == 0xD9) break;                              // EOI
        if (p + 3 >= d.size()) break;
        uint32_t len = (d[p + 2] << 8) | d[p + 3];
        const uint8_t* s = d.data() + p + 4;
        uint32_t slen = len - 2;
        if (p + 2 + len > d.size()) return false;          // truncated

        switch (m) {
        case 0xDB: { // DQT
            uint32_t o = 0;
            while (o < slen) {
                int pq = s[o] >> 4, tq = s[o] & 15; o++;
                if (tq > 3 || pq > 1 || o + (pq ? 128u : 64u) > slen) return false;
                for (int i = 0; i < 64; ++i) {
                    uint16_t v = pq ? (s[o] << 8) | s[o + 1] : s[o];
                    o += pq ? 2 : 1;
                    j.qt[tq][kZig[i]] = v;   // -> natural order
                }
            }
            break;
        }
        case 0xC4: { // DHT (possibly several tables per segment)
            uint32_t o = 0;
            while (o < slen) {
                if (o + 17 > slen) return false;
                int tc = s[o] >> 4, th = s[o] & 15; o++;
                if (th > 3 || tc > 1) return false;
                HuffTable& t = tc ? j.actab[th] : j.dctab[th];
                int n = 0;
                for (int i = 0; i < 16; ++i) { t.counts[i] = s[o + i]; n += t.counts[i]; }
                o += 16;
                if (o + uint32_t(n) > slen || n > 256) return false;
                t.vals.assign(s + o, s + o + n);
                o += n;
                j.haveDht = true;
            }
            break;
        }
        case 0xC0: case 0xC1: { // SOF0 baseline / SOF1 ext sequential
            if (s[0] != 8) {
                fprintf(stderr, "unsupported precision %d\n", s[0]);
                return false;
            }
            j.H = (s[1] << 8) | s[2];
            j.W = (s[3] << 8) | s[4];
            j.nComp = s[5];
            if (j.nComp != 1 && j.nComp != 3) {
                fprintf(stderr, "unsupported %d components\n", j.nComp);
                return false;
            }
            for (int i = 0; i < j.nComp; ++i) {
                Comp& c = j.comps[i];
                c.id = s[6 + i * 3];
                c.hs = s[7 + i * 3] >> 4;
                c.vs = s[7 + i * 3] & 15;
                c.tq = s[8 + i * 3];
                j.maxH = std::max(j.maxH, c.hs);
                j.maxV = std::max(j.maxV, c.vs);
            }
            haveSof = true;
            break;
        }
        case 0xC2: {
            fprintf(stderr, "progressive JPEG not supported (baseline only)\n");
            return false;
        }
        case 0xDD: // DRI
            j.dri = (s[0] << 8) | s[1];
            break;
        case 0xDA: { // SOS — entropy data follows the header
            int ns = s[0];
            if (ns != j.nComp) {
                fprintf(stderr, "multi-scan SOS unsupported\n");
                return false;
            }
            for (int i = 0; i < ns; ++i) {
                int cs = s[1 + i * 2];
                Comp* c = nullptr;
                for (auto& cc : j.comps) if (cc.id == cs) c = &cc;
                if (!c) return false;
                c->td = s[2 + i * 2] >> 4;
                c->ta = s[2 + i * 2] & 15;
                if (c->td > 3 || c->ta > 3) return false;
            }
            j.scanOff = uint32_t(p + 4 + slen);
            // entropy data ends before the next unstuffed marker
            size_t q = j.scanOff;
            while (q + 1 < d.size()) {
                if (d[q] == 0xFF && d[q + 1] != 0x00 &&
                    !(d[q + 1] >= 0xD0 && d[q + 1] <= 0xD7))
                    break;
                q++;
            }
            j.scanLen = uint32_t(q - j.scanOff);
            p = q;
            goto scanDone;
        }
        default: break;
        }
        p += 2 + len;
    }
scanDone:
    if (!haveSof || !j.haveDht || !j.scanLen) {
        fprintf(stderr, "missing SOF/DHT/SOS\n");
        return false;
    }

    // ---- geometry ----
    j.mcusX = (j.W + 8 * j.maxH - 1) / (8 * j.maxH);
    j.mcusY = (j.H + 8 * j.maxV - 1) / (8 * j.maxV);
    j.totalMcus = j.mcusX * j.mcusY;
    for (int i = 0; i < j.nComp; ++i) {
        Comp& c = j.comps[i];
        c.bw = j.mcusX * c.hs;
        c.bh = j.mcusY * c.vs;
        c.w = (j.W * c.hs + j.maxH - 1) / j.maxH;
        c.h = (j.H * c.vs + j.maxV - 1) / j.maxV;
    }

    // ---- restart intervals: scan for FFD0..FFD7 boundaries ----
    j.intervals.push_back({0, 0, 0});
    const uint8_t* sd = d.data() + j.scanOff;
    int found = 0;
    for (uint32_t i = 0; i + 1 < j.scanLen; ++i) {
        if (sd[i] == 0xFF && sd[i + 1] >= 0xD0 && sd[i + 1] <= 0xD7) {
            found++;
            j.intervals.back().nMcu =
                uint32_t(found * std::max(j.dri, 1)) - j.intervals.back().mcuStart;
            j.intervals.push_back({i + 2, uint32_t(found) * std::max(j.dri, 1), 0});
            i++;
        }
    }
    if (found == 0) {
        // no markers (DRI=0 or missing): one interval for everything
        j.intervals.resize(1);
        j.intervals[0] = {0, 0, uint32_t(j.totalMcus)};
    } else {
        j.intervals.back().nMcu =
            uint32_t(j.totalMcus) - j.intervals.back().mcuStart;
    }
    return true;
}

// ============================================================
//  GPU buffer layouts (std430, plain int32 words throughout)
// ============================================================

// Tables buffer words:
//   [0x000..0x0FF]  qtab: 4 x 64 natural-order quant values
//   per table t (8 total: 0-3 DC, 4-7 AC), base = 256 + t*816:
//     +0    mincode[16]   +16  maxcode[16]   +32  valptr[16]
//     +48   lut8[256]     +304 huffval[256]
static constexpr int kTabWords = 256 + 8 * 816;
static constexpr int kQBase = 0, kHuffBase = 256;

// Flat 16-bit "fused" Huffman LUT — one lookup yields the symbol AND,
// whenever huffLen+extLen fits in the 16-bit window, the already
// sign-extended coefficient value. Entry (u32):
//   bit31 set   -> fused: [4:0]=totalLen [8:5]=r [12:9]=s [28:13]=val+32768
//   bit31 clear -> plain: [4:0]=huffLen [15:8]=sym   (huffLen==0 -> invalid)
static constexpr int kFLutWords = 8 * 65536;

static int extendRef(int v, int s) {
    return (s > 0 && v < (1 << (s - 1))) ? v - (1 << s) + 1 : v;
}

static void packFLut(const Jpg& j, std::vector<uint32_t>& fl) {
    fl.assign(kFLutWords, 0);
    for (int t = 0; t < 8; ++t) {
        const HuffTable& h = (t < 4) ? j.dctab[t] : j.actab[t - 4];
        for (int x = 0; x < 65536; ++x) {
            // canonical walk for the first code in this prefix
            int len = 0, sym = 0;
            for (int l = 1; l <= 16; ++l) {
                int code = x >> (16 - l);
                if (h.maxcode[l - 1] >= 0 && code <= h.maxcode[l - 1]) {
                    len = l;
                    sym = h.vals[h.valptr[l - 1] + code];
                    break;
                }
            }
            if (!len) continue;
            int r = (t < 4) ? 0 : (sym >> 4), s = sym & 15;
            if (t < 4) s = sym;                    // DC: symbol is size
            if (len + s <= 16) {
                int ext = (x >> (16 - len - s)) & ((1 << s) - 1);
                int val = extendRef(ext, s);
                fl[t * 65536 + x] = 0x80000000u | uint32_t(len + s) |
                                    uint32_t(r << 5) | uint32_t(s << 9) |
                                    uint32_t((val + 32768) << 13);
            } else {
                fl[t * 65536 + x] = uint32_t(len) | uint32_t(sym << 8);
            }
        }
    }
}

static void packTables(const Jpg& j, std::vector<int32_t>& tab) {
    tab.assign(kTabWords, 0);
    for (int t = 0; t < 4; ++t)
        for (int i = 0; i < 64; ++i)
            tab[kQBase + t * 64 + i] = j.qt[t][i];
    for (int t = 0; t < 8; ++t) {
        const HuffTable& h = (t < 4) ? j.dctab[t] : j.actab[t - 4];
        int b = kHuffBase + t * 816;
        for (int i = 0; i < 16; ++i) {
            tab[b + i] = h.mincode[i];
            tab[b + 16 + i] = h.maxcode[i];
            tab[b + 32 + i] = h.valptr[i];
        }
        for (int i = 0; i < 256; ++i) {
            tab[b + 48 + i] = int32_t(h.lut8[i]);
            tab[b + 304 + i] = i < (int)h.vals.size() ? h.vals[i] : 0;
        }
    }
}

// Per-component params, 16 int32 words each (binding 4, both recon and
// upsample read it):
//   0 hs  1 vs  2 bw  3 bh  4 w  5 h  6 qt  7 dcTab  8 acTab
//   9 coefOfs(words)  10 planeOfs(bytes)  11 pad..
struct FrameGpu {
    int32_t comps[4][16];
    int32_t totalBlocks, planeWords, nIntervals;
};

static FrameGpu packComps(const Jpg& j) {
    FrameGpu f{};
    uint32_t coefOf = 0, planeOf = 0;
    for (int i = 0; i < 4; ++i) {
        const Comp& c = j.comps[i];
        f.comps[i][0] = c.hs;  f.comps[i][1] = c.vs;
        f.comps[i][2] = c.bw;  f.comps[i][3] = c.bh;
        f.comps[i][4] = c.w;   f.comps[i][5] = c.h;
        f.comps[i][6] = c.tq;
        f.comps[i][7] = c.td;          // dctab[t] -> tabs[t]
        f.comps[i][8] = 4 + c.ta;      // actab[t] -> tabs[4+t]
        f.comps[i][9] = int32_t(coefOf);
        f.comps[i][10] = int32_t(planeOf);
        if (i < j.nComp) {
            coefOf += uint32_t(c.bw) * c.bh * 32;      // 64 coefs / 2 per word
            planeOf += uint32_t(c.bw * 8) * (c.bh * 8);    // plane bytes
        }
        f.totalBlocks += c.bw * c.bh;
    }
    f.planeWords = int32_t(planeOf / 4);
    f.nIntervals = int32_t(j.intervals.size());
    return f;
}

// ============================================================
//  Shader 1 — entropy decode: one invocation per restart interval
// ============================================================
static const char* kEntropy = R"GLSL(
#version 460
layout(local_size_x = 32) in;

layout(set = 0, binding = 0) readonly buffer Scan { uint scan[]; };
layout(set = 0, binding = 1) readonly buffer Chunks { uvec4 ck[]; };
layout(set = 0, binding = 2) uniform Tabs { uvec4 tab[1696]; };
#define T(i) int(tab[(i) >> 2][(i) & 3])
layout(set = 0, binding = 3) buffer State { uint st[]; };
layout(set = 0, binding = 4) uniform Comps { ivec4 comps[4][4]; };
#define CM(c, i) (comps[c][(i) >> 2][(i) & 3])
layout(set = 0, binding = 8) readonly buffer FLut { int flut[]; };
layout(set = 0, binding = 5) writeonly buffer Coef { uint coef[]; };
layout(push_constant) uniform P { uint W, H, mcusX, nComp, maxH, maxV, nIv, scanLen, genOfs; };

const uint kTabBase = 256;          // table t base = 256 + t*816

// --- bit reader: MSB-first, byte-packed words in scan[] ---
struct BR { uint pos; uint buf; int cnt; bool end; uint wpos; uint wval;
            uint nval; };

uint byteAt(uint p) {
    return (scan[p >> 2] >> ((p & 3u) * 8u)) & 0xFFu;
}

uint rdByte(inout BR b) {
    if (b.end) return 0xFFu;                       // pad 1s
    uint wp = b.pos >> 2;
    if (wp != b.wpos) {
        // rotate in prefetched next word, prefetch the following one
        b.wval = (wp == b.wpos + 1u) ? b.nval : scan[wp];
        b.nval = (wp + 1u < ((scanLen + 3u) >> 2)) ? scan[wp + 1u] : 0u;
        b.wpos = wp;
    }
    uint v = (b.wval >> ((b.pos & 3u) * 8u)) & 0xFFu;
    if (v != 0xFFu) { b.pos++; return v; }
    // 0xFF: crawl over fill-byte run (spec allows FF+ before markers),
    // then classify the byte that follows (jdhuff.c jpeg_fill_bit_buffer)
    uint p = b.pos + 1;
    uint n = 0xFFu;
    while (n == 0xFFu && p < scanLen) {
        n = byteAt(p);
        if (n == 0xFFu) p++;
    }
    if (n == 0u) { b.pos = p + 1; b.wpos = 0xFFFFFFFEu; return 0xFFu; }
    b.end = true;                                  // marker: stop
    return 0xFFu;
}
void fill(inout BR b) {
    while (b.cnt <= 24) { b.buf = (b.buf << 8) | rdByte(b); b.cnt += 8; }
}
uint peekN(inout BR b, uint n) {
    fill(b);
    if (uint(b.cnt) >= n) return (b.buf >> (uint(b.cnt) - n)) & ((1u << n) - 1u);
    uint real = uint(b.cnt);
    uint v = (b.buf & ((1u << real) - 1u)) << (n - real);
    return v | ((1u << (n - real)) - 1u);
}
void eat(inout BR b, uint n) {
    b.cnt = (uint(b.cnt) >= n) ? b.cnt - int(n) : 0;
    b.buf &= (b.cnt >= 32) ? 0xFFFFFFFFu : ((1u << uint(b.cnt)) - 1u);
}

int rext(inout BR b, int s) {          // receive + extend fused
    if (s == 0) return 0;
    uint v = peekN(b, uint(s)); eat(b, uint(s));
    int i = int(v);
    int vt = 1 << (s - 1);
    return (i < vt) ? i - (1 << s) + 1 : i;
}

void main() {
    if (gl_GlobalInvocationID.x >= nIv) return;   // dispatch overrun
    uint idx = genOfs + gl_GlobalInvocationID.x;  // work item = (iv,chunk) grid
    uvec4 w = ck[idx];
    if (w.z == 0u) return;                        // padded grid cell
    BR b;
    ivec4 pred;
    if (w.w == 0u) {                              // first chunk of interval
        b.pos = w.x; b.buf = 0u; b.cnt = 0; b.end = false;
        b.wpos = 0xFFFFFFFEu; b.wval = 0u; b.nval = 0u;
        pred = ivec4(0);
    } else {                                      // resume predecessor's state
        uint s = (idx - nIv) * 8u;
        b.pos = st[s]; b.buf = st[s + 1]; b.cnt = int(st[s + 2]);
        b.end = st[s + 3] != 0u;
        b.wpos = 0xFFFFFFFEu; b.wval = 0u; b.nval = 0u;
        pred = ivec4(int(st[s + 4]), int(st[s + 5]),
                     int(st[s + 6]), int(st[s + 7]));
    }

    int mcusY = (int(H) + 8 * int(maxV) - 1) / (8 * int(maxV));
    for (uint m = 0; m < w.z; ++m) {
        int mcu = int(w.y + m);
        if (mcu >= int(mcusX) * mcusY) break;   // malformed interval table
        int mx = mcu % int(mcusX), my = mcu / int(mcusX);
        for (uint c = 0; c < nComp; ++c) {
            int hs = CM(c, 0), vs = CM(c, 1);
            int bw = CM(c, 2);
            int dt = CM(c, 7), at = CM(c, 8);
            uint cofs = uint(CM(c, 9));
            for (int v = 0; v < vs; ++v)
            for (int h = 0; h < hs; ++h) {
                int bi = (my * vs + v) * bw + mx * hs + h;
                uint outp = cofs + uint(bi) * 32u;
                // coef buffer is pre-zeroed (vkCmdFillBuffer): stores
                // below accumulate one pending word per block (pw/pv)
                // -- DC: fused entry -> sign-extended diff in one lookup
                int e = flut[uint(dt) * 65536u + peekN(b, 16u)];
                bool doAc = true;
                int diff;
                if (e < 0) {
                    eat(b, uint(e & 0x1F));
                    diff = ((e >> 13) & 0xFFFF) - 32768;
                } else {
                    int len = e & 0x1F;
                    if (len == 0) { doAc = false; diff = 0; }
                    else { eat(b, uint(len)); diff = rext(b, (e >> 8) & 0xFF); }
                }
                pred[c] += diff;
                uint pw = 0u, pv = uint(pred[c]) & 0xFFFFu;
                // -- AC: ZRL/EOB need no stores; nonzero coefs gather
                //    into the pending word
                if (doAc) {
                    int k = 1;
                    while (k < 64) {
                        e = flut[uint(at) * 65536u + peekN(b, 16u)];
                        int rr, ss, val;
                        if (e < 0) {
                            eat(b, uint(e & 0x1F));
                            rr = (e >> 5) & 0xF; ss = (e >> 9) & 0xF;
                            val = ((e >> 13) & 0xFFFF) - 32768;
                        } else {
                            int len = e & 0x1F;
                            if (len == 0) break;
                            eat(b, uint(len));
                            int sym = (e >> 8) & 0xFF;
                            rr = sym >> 4; ss = sym & 15;
                            val = (ss > 0) ? rext(b, ss) : 0;
                        }
                        if (ss == 0) {
                            if (rr == 15) { k += 16; continue; }
                            break;                // EOB
                        }
                        k += rr;
                        if (k < 64) {
                            uint wv = uint(k >> 1);
                            if (wv != pw) { coef[outp + pw] = pv; pw = wv; pv = 0u; }
                            pv |= (uint(val) & 0xFFFFu) << ((k & 1) * 16);
                        }
                        k++;
                    }
                }
                coef[outp + pw] = pv;
            }
        }
    }

    // hand bitstate + DC predictors to the next generation's chunk
    uint s = idx * 8u;
    st[s] = b.pos; st[s + 1] = b.buf; st[s + 2] = uint(b.cnt);
    st[s + 3] = b.end ? 1u : 0u;
    st[s + 4] = uint(pred[0]); st[s + 5] = uint(pred[1]);
    st[s + 6] = uint(pred[2]); st[s + 7] = uint(pred[3]);
}
)GLSL";

// ============================================================
//  Shader 2 — block reconstruction: dequant + islow int IDCT
//  (libjpeg-turbo jidctint.c, CONST_BITS=13 PASS1_BITS=2 — bit-exact)
// ============================================================
static const char* kRecon = R"GLSL(
#version 460
layout(local_size_x = 64) in;

layout(set = 0, binding = 2) uniform Tabs { uvec4 tab[1696]; };
#define T(i) int(tab[(i) >> 2][(i) & 3])
layout(set = 0, binding = 4) uniform Comps { ivec4 comps[4][4]; };
#define CM(c, i) (comps[c][(i) >> 2][(i) & 3])
layout(set = 0, binding = 5) readonly buffer Coef { uint coef[]; };
layout(set = 0, binding = 6) writeonly buffer Planes { uint planes[]; };
layout(push_constant) uniform P { uint W, H, mcusX, nComp, maxH, maxV, nIv, scanLen; };

const int zig[64] = int[64](
    0,1,8,16,9,2,3,10,17,24,32,25,18,11,4,5,
    12,19,26,33,40,48,41,34,27,20,13,6,7,14,21,28,
    35,42,49,56,57,50,43,36,29,22,15,23,30,37,44,51,
    58,59,52,45,38,31,39,46,53,60,61,54,47,55,62,63);

int desc(int x, int n) { return (x + (1 << (n - 1))) >> n; }

// one full islow pass over 8 values (shared by both directions)
// idx selects the 8 input strided positions; returns via ws[]
void idct1d(in int inp[8], out int outp[8], int sc, int nsh) {
    int z2 = inp[2] * sc, z3 = inp[6] * sc;
    int z1 = (z2 + z3) * 4433;                    // FIX_0_541196100
    int t2 = z1 - z3 * 15137;                     // FIX_1_847759065
    int t3 = z1 + z2 * 6270;                      // FIX_0_765366865
    z2 = inp[0] * sc; z3 = inp[4] * sc;
    int t0 = (z2 + z3) << 13, t1 = (z2 - z3) << 13;
    int t10 = t0 + t3, t13 = t0 - t3;
    int t11 = t1 + t2, t12 = t1 - t2;
    t0 = inp[7] * sc; t1 = inp[5] * sc;
    t2 = inp[3] * sc; t3 = inp[1] * sc;
    z1 = t0 + t3; z2 = t1 + t2;
    int z3o = t0 + t2, z4 = t1 + t3;
    int z5 = (z3o + z4) * 9633;                   // FIX_1_175875602
    t0 *= 2446;  t1 *= 16819; t2 *= 25172; t3 *= 12299;
    z1 *= -7373; z2 *= -20995; z3o *= -16069; z4 *= -3196;
    z3o += z5; z4 += z5;
    t0 += z1 + z3o; t1 += z2 + z4;
    t2 += z2 + z3o; t3 += z1 + z4;
    outp[0] = desc(t10 + t3, nsh); outp[7] = desc(t10 - t3, nsh);
    outp[1] = desc(t11 + t2, nsh); outp[6] = desc(t11 - t2, nsh);
    outp[2] = desc(t12 + t1, nsh); outp[5] = desc(t12 - t1, nsh);
    outp[3] = desc(t13 + t0, nsh); outp[4] = desc(t13 - t0, nsh);
}

void main() {
    uint gid = gl_GlobalInvocationID.x;
    // find which component this block belongs to (bounded — dispatch
    // overrun invocations must not walk past the last component)
    int c = 0;
    while (c < int(nComp) - 1 && gid >= uint(CM(c, 2) * CM(c, 3))) {
        gid -= uint(CM(c, 2) * CM(c, 3)); c++;
    }
    if (c >= int(nComp) || int(gid) >= CM(c, 2) * CM(c, 3)) return;
    int bw = CM(c, 2);
    int bx = int(gid) % bw, by = int(gid) / bw;
    int qt = CM(c, 6);
    uint cofs = uint(CM(c, 9));
    uint pofs = uint(CM(c, 10)) >> 2;        // bytes -> words

    // dequantize (zigzag buffer -> natural order)
    int deq[64];
    for (int i = 0; i < 64; i += 2) {
        uint w2 = coef[cofs + uint(bx + by * bw) * 32u + uint(i / 2)];
        int v0 = int(w2 & 0xFFFFu); if (v0 >= 32768) v0 -= 65536;
        int v1 = int(w2 >> 16);     if (v1 >= 32768) v1 -= 65536;
        deq[zig[i]]     = v0 * T(qt * 64 + zig[i]);
        deq[zig[i + 1]] = v1 * T(qt * 64 + zig[i + 1]);
    }

    // pass 1: columns (dequant inside the pass, like jidctint)
    int ws[64];
    for (int col = 0; col < 8; ++col) {
        int inp[8], outp[8];
        for (int r = 0; r < 8; ++r) inp[r] = deq[r * 8 + col];
        idct1d(inp, outp, 1, 11);                 // scale 1, descale 11
        for (int r = 0; r < 8; ++r) ws[r * 8 + col] = outp[r];
    }
    // pass 2: rows -> u8, pack 4 per word
    for (int r = 0; r < 8; ++r) {
        int inp[8], outp[8];
        for (int cc = 0; cc < 8; ++cc) inp[cc] = ws[r * 8 + cc];
        idct1d(inp, outp, 1, 18);                 // descale 18
        uint base = pofs + uint(by * 8 + r) * uint(bw * 2) + uint(bx * 2);
        uint w0 = 0u, w1 = 0u;
        for (int cc = 0; cc < 4; ++cc) {
            int a = clamp(outp[cc] + 128, 0, 255);
            int b = clamp(outp[cc + 4] + 128, 0, 255);
            w0 |= uint(a) << (cc * 8);
            w1 |= uint(b) << (cc * 8);
        }
        planes[base] = w0;
        planes[base + 1] = w1;
    }
}
)GLSL";

// ============================================================
//  Shader 3 — upsample + color convert (per output pixel)
//  Mirrors jdsample.c fancy upsampling (bias +1/+2, +7/+8) and
//  jdcolor.c fixed-point YCbCr->RGB — target: bit-exact vs libjpeg.
// ============================================================
static const char* kUpsample = R"GLSL(
#version 460
layout(local_size_x = 16, local_size_y = 16) in;

layout(set = 0, binding = 4) uniform Comps { ivec4 comps[4][4]; };
#define CM(c, i) (comps[c][(i) >> 2][(i) & 3])
layout(set = 0, binding = 6) readonly buffer Planes { uint planes[]; };
layout(set = 0, binding = 7) writeonly buffer OutB { uint outp[]; };
layout(push_constant) uniform P { uint W, H, mcusX, nComp, maxH, maxV, nIv, scanLen; };

// fetch one u8 sample from component c's packed plane
int smp(int c, int x, int y) {
    uint pofs = uint(CM(c, 10));
    int stride = CM(c, 2) * 8;               // samples per plane row
    uint a = pofs + uint(y * stride + x);
    return int((planes[a >> 2] >> ((a & 3u) * 8u)) & 0xFFu);
}

int clampi(int v) { return clamp(v, 0, 255); }

// upsampled component value at output pixel (x,y)
int usamp(int c, int x, int y) {
    int hs = CM(c, 0), vs = CM(c, 1);
    int cw = CM(c, 4), ch = CM(c, 5);
    int rh = int(maxH) / hs, rv = int(maxV) / vs;
    if (rh == 1 && rv == 1)
        return smp(c, x, y);
    if (rh == 2 && rv == 1) {                    // h2v1 fancy
        int j = clamp(x >> 1, 0, cw - 1);
        if (x == 0) return smp(c, 0, y);
        if (x == cw * 2 - 1) return smp(c, cw - 1, y);
        if ((x & 1) == 0)
            return (3 * smp(c, j, y) + smp(c, j - 1, y) + 1) >> 2;
        return (3 * smp(c, j, y) + smp(c, j + 1, y) + 2) >> 2;
    }
    if (rh == 1 && rv == 2) {                    // h1v2 fancy
        int i = clamp(y >> 1, 0, ch - 1);
        int adj = clamp(i + (((y & 1) == 0) ? -1 : 1), 0, ch - 1);
        int bias = ((y & 1) == 0) ? 1 : 2;
        return (3 * smp(c, x, i) + smp(c, x, adj) + bias) >> 2;
    }
    if (rh == 2 && rv == 2 && cw > 2) {          // h2v2 fancy
        int i = clamp(y >> 1, 0, ch - 1);
        int i2 = clamp(i + (((y & 1) == 0) ? -1 : 1), 0, ch - 1);
        int j = clamp(x >> 1, 0, cw - 1);
        int this0 = 3 * smp(c, j, i) + smp(c, j, i2);   // colsum j
        if (x == 0) return (this0 * 4 + 8) >> 4;
        if (x == cw * 2 - 1) return (this0 * 4 + 7) >> 4;
        int j2 = clamp(j + (((x & 1) == 0) ? -1 : 1), 0, cw - 1);
        int adj0 = 3 * smp(c, j2, i) + smp(c, j2, i2);
        int bias = ((x & 1) == 0) ? 8 : 7;
        return (this0 * 3 + adj0 + bias) >> 4;
    }
    // integral factors: plain replication
    int cx = clamp(x / rh, 0, cw - 1);
    int cy = clamp(y / rv, 0, ch - 1);
    return smp(c, cx, cy);
}

void main() {
    uint x = gl_GlobalInvocationID.x, y = gl_GlobalInvocationID.y;
    if (x >= W || y >= H) return;
    int r, g, bl;
    if (nComp == 1u) {
        r = g = bl = usamp(0, int(x), int(y));
    } else {
        int yy = usamp(0, int(x), int(y));
        int cb = usamp(1, int(x), int(y)) - 128;
        int cr = usamp(2, int(x), int(y)) - 128;
        r = clampi(yy + ((91881 * cr + 32768) >> 16));
        g = clampi(yy + ((32768 - 22554 * cb - 46802 * cr) >> 16));
        bl = clampi(yy + ((116130 * cb + 32768) >> 16));
    }
    outp[y * W + x] = 0xFF000000u | uint(bl) << 16 | uint(g) << 8 | uint(r);
}
)GLSL";

// ============================================================
//  Vulkan plumbing
// ============================================================

struct Gpu {
    vkmini::Vk v;
    vk::UniqueDescriptorSetLayout dsetLayout;
    vk::UniqueDescriptorPool dpool;
    vk::DescriptorSet dset;
    vk::UniquePipelineLayout pipeLayout;
    vk::UniquePipeline pEntropy, pRecon, pUpsample;

    void init() {
        v.createInstance({});
        v.pickPhysicalDevice();
        v.createDevice();
        std::vector<vk::DescriptorSetLayoutBinding> binds;
        for (uint32_t i = 0; i < 9; ++i)
            binds.push_back({i,
                             (i == 2 || i == 4)
                                 ? vk::DescriptorType::eUniformBuffer
                                 : vk::DescriptorType::eStorageBuffer,
                             1, vk::ShaderStageFlagBits::eCompute});
        dsetLayout = v.device->createDescriptorSetLayoutUnique({{}, binds});
        std::vector<vk::DescriptorPoolSize> sz = {
            {vk::DescriptorType::eStorageBuffer, 9},
            {vk::DescriptorType::eUniformBuffer, 2}};
        dpool = v.device->createDescriptorPoolUnique(
            {vk::DescriptorPoolCreateFlagBits::eFreeDescriptorSet, 1, sz});
        dset = v.device->allocateDescriptorSets(
            {dpool.get(), dsetLayout.get()}).front();
        vk::PushConstantRange pc{vk::ShaderStageFlagBits::eCompute, 0, 36};
        pipeLayout = v.device->createPipelineLayoutUnique(
            {{}, dsetLayout.get(), pc});
        auto mk = [&](const char* src, const char* name) {
            auto mod = v.shader(src, shaderc_compute_shader, name);
            vk::ComputePipelineCreateInfo ci{};
            ci.stage = {vk::PipelineShaderStageCreateFlags{},
                        vk::ShaderStageFlagBits::eCompute, mod.get(), "main"};
            ci.layout = pipeLayout.get();
            return v.device->createComputePipelineUnique({}, ci).value;
        };
        pEntropy = mk(kEntropy, "entropy");
        pRecon = mk(kRecon, "recon");
        pUpsample = mk(kUpsample, "upsample");
    }

    vkmini::Vk::Buffer ssbo(vk::DeviceSize bytes, bool map = false,
                            vk::BufferUsageFlags extra = {}) {
        return v.createBuffer(
            bytes, vk::BufferUsageFlagBits::eStorageBuffer | extra,
            vk::MemoryPropertyFlagBits::eHostVisible |
                vk::MemoryPropertyFlagBits::eHostCoherent,
            map);
    }

    void bindBuf(uint32_t binding, const vkmini::Vk::Buffer& b,
                 bool ubo = false) {
        vk::DescriptorBufferInfo bi{b.buf.get(), 0, b.size};
        vk::WriteDescriptorSet w{dset, binding, 0, 1,
                                 ubo ? vk::DescriptorType::eUniformBuffer
                                     : vk::DescriptorType::eStorageBuffer,
                                 nullptr, &bi};
        v.device->updateDescriptorSets(w, {});
    }

    void barrier(vk::CommandBuffer c) {
        vk::MemoryBarrier mb{vk::AccessFlagBits::eShaderWrite,
                             vk::AccessFlagBits::eShaderRead};
        c.pipelineBarrier(vk::PipelineStageFlagBits::eComputeShader,
                          vk::PipelineStageFlagBits::eComputeShader,
                          {}, mb, {}, {});
    }
};


// ============================================================
//  C ABI for ImageCropper — decodeInto + probe/decode exports.
//  Vendored from VulkanExamples/p80_gpujpegdec/main.cpp; the CLI
//  main()/libjpeg verify path lives there, this file is the library.
// ============================================================

static void decodeInto(Gpu& g, const Jpg& j, const FrameGpu& fg,
                       uint8_t* out) {
    auto scanBuf = g.ssbo((j.scanLen + 3) & ~3u, true);
    memcpy(scanBuf.mapped, j.data.data() + j.scanOff, j.scanLen);

    // work items: nIntervals x nGen grid, item (i,k) = chunk k of
    // interval i. Chunk state chains via st[item] -> st[item+nIv].
    const uint32_t kChunkMcus = 2048;
    uint32_t nGen = 1;
    for (const auto& iv : j.intervals)
        nGen = std::max(nGen, (iv.nMcu + kChunkMcus - 1) / kChunkMcus);
    const uint32_t nIv = uint32_t(j.intervals.size());
    std::vector<uint32_t> ck(size_t(nIv) * nGen * 4, 0);
    for (uint32_t gI = 0; gI < nGen; ++gI)
        for (uint32_t i = 0; i < nIv; ++i) {
            const Interval& iv = j.intervals[i];
            uint32_t ms = iv.mcuStart + gI * kChunkMcus;
            int64_t nm = int64_t(iv.mcuStart) + iv.nMcu - int64_t(ms);
            nm = std::clamp<int64_t>(nm, 0, kChunkMcus);
            uint32_t* w = &ck[(size_t(gI) * nIv + i) * 4];
            w[0] = gI == 0 ? iv.byteStart : 0;
            w[1] = ms;
            w[2] = uint32_t(nm);
            w[3] = gI > 0 ? 1u : 0u;
        }
    auto ivBuf = g.ssbo(ck.size() * 4, true);
    memcpy(ivBuf.mapped, ck.data(), ck.size() * 4);

    auto stBuf = g.ssbo(size_t(nIv) * nGen * 8 * 4);

    auto tabBuf = g.ssbo(kTabWords * 4, true,
                         vk::BufferUsageFlagBits::eUniformBuffer);
    {
        std::vector<int32_t> tab;
        packTables(j, tab);
        memcpy(tabBuf.mapped, tab.data(), tab.size() * 4);
    }

    auto flutBuf = g.ssbo(kFLutWords * 4, true);
    {
        std::vector<uint32_t> fl;
        packFLut(j, fl);
        memcpy(flutBuf.mapped, fl.data(), fl.size() * 4);
    }

    auto compBuf = g.ssbo(sizeof(fg.comps), true,
                          vk::BufferUsageFlagBits::eUniformBuffer);
    memcpy(compBuf.mapped, fg.comps, sizeof(fg.comps));

    auto coefBuf = g.ssbo(size_t(fg.totalBlocks) * 32 * 4, false,
                          vk::BufferUsageFlagBits::eTransferDst);
    auto planeBuf = g.ssbo(size_t(fg.planeWords) * 4);
    auto outBuf = g.ssbo(size_t(j.W) * j.H * 4, true);

    g.bindBuf(0, scanBuf);
    g.bindBuf(1, ivBuf);
    g.bindBuf(2, tabBuf, true);
    g.bindBuf(3, stBuf);
    g.bindBuf(4, compBuf, true);
    g.bindBuf(5, coefBuf);
    g.bindBuf(6, planeBuf);
    g.bindBuf(7, outBuf);
    g.bindBuf(8, flutBuf);

    uint32_t pc[9] = {uint32_t(j.W),   uint32_t(j.H),    uint32_t(j.mcusX),
                      uint32_t(j.nComp), uint32_t(j.maxH), uint32_t(j.maxV),
                      nIv,               j.scanLen,        0u};

    // one submit per chunk-generation: bounds per-job GPU time so a
    // long serial scan can't trip the driver/kernel watchdog
    for (uint32_t gI = 0; gI < nGen; ++gI) {
        pc[8] = gI * nIv;
        g.v.oneTime([&](vk::CommandBuffer c) {
            if (gI == 0) {
                c.fillBuffer(coefBuf.buf.get(), 0, coefBuf.size, 0);
                vk::MemoryBarrier mb{
                    vk::AccessFlagBits::eTransferWrite,
                    vk::AccessFlagBits::eShaderRead |
                        vk::AccessFlagBits::eShaderWrite};
                c.pipelineBarrier(
                    vk::PipelineStageFlagBits::eTransfer,
                    vk::PipelineStageFlagBits::eComputeShader, {},
                    mb, {}, {});
            }
            c.pushConstants(g.pipeLayout.get(),
                            vk::ShaderStageFlagBits::eCompute, 0, 36, pc);
            c.bindDescriptorSets(vk::PipelineBindPoint::eCompute,
                                 g.pipeLayout.get(), 0, g.dset, {});
            c.bindPipeline(vk::PipelineBindPoint::eCompute,
                           g.pEntropy.get());
            g.barrier(c);
            c.dispatch((nIv + 31) / 32, 1, 1);
        });
    }
    g.v.oneTime([&](vk::CommandBuffer c) {
        c.pushConstants(g.pipeLayout.get(),
                        vk::ShaderStageFlagBits::eCompute, 0, 36, pc);
        c.bindDescriptorSets(vk::PipelineBindPoint::eCompute,
                             g.pipeLayout.get(), 0, g.dset, {});
        g.barrier(c);
        c.bindPipeline(vk::PipelineBindPoint::eCompute, g.pRecon.get());
        c.dispatch((fg.totalBlocks + 63) / 64, 1, 1);
        g.barrier(c);
        c.bindPipeline(vk::PipelineBindPoint::eCompute, g.pUpsample.get());
        c.dispatch((j.W + 15) / 16, (j.H + 15) / 16, 1);
    });

    memcpy(out, outBuf.mapped, size_t(j.W) * j.H * 4);
}

static Gpu& gpuCtx() {
    static Gpu g;
    static int ok = -1;
    if (ok < 0) {
        try { g.init(); ok = 1; }
        catch (...) { ok = 0; }
    }
    if (!ok) throw std::runtime_error("vulkan init failed");
    return g;
}

extern "C" {

// Probe JPEG headers without touching the GPU.
//   out: w, h, dri (restart interval in MCUs, 0 = none)
//   returns 0 if this is a decodable baseline JPEG, 1 otherwise
int gpujpeg_probe(const uint8_t* data, size_t len,
                  int* w, int* h, int* dri) {
    Jpg j;
    j.data.assign(data, data + len);
    if (!parseJpg(j)) return 1;
    *w = j.W; *h = j.H; *dri = j.dri;
    return 0;
}

// Decode JPEG -> RGBA8 (out must be w*h*4 bytes).
//   require_restart: reject DRI==0 files (serial GPU path is slower
//                    than a CPU decoder there — caller should fall back)
//   returns 0 ok, 1 unsupported/slow (caller uses CPU), <0 error
int gpujpeg_decode(const uint8_t* data, size_t len,
                   uint8_t* out, int require_restart) {
    Jpg j;
    j.data.assign(data, data + len);
    if (!parseJpg(j)) return 1;
    if (require_restart && j.dri == 0) return 1;
    try {
        Gpu& g = gpuCtx();
        for (auto& t : j.dctab) buildCanon(t);
        for (auto& t : j.actab) buildCanon(t);
        FrameGpu fg = packComps(j);
        decodeInto(g, j, fg, out);
        return 0;
    } catch (...) {
        return -1;
    }
}

}
