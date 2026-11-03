//! WGSL compute shaders — ports of the GLSL in the old
//! native/gpujpegdec.cpp and VulkanExamples/p83_progjpegdec.
//! All math is integer-only, so the port is bit-exact by construction
//! (no fp rounding concerns anywhere in the decode or resample path).

/// Shared decode pipeline: 9 bindings, up to 88B push constants.
pub const ENTROPY: &str = r#"
struct Comps { c: array<array<vec4<i32>, 4>, 4>, }
struct Pc { w: u32, h: u32, mcus_x: u32, n_comp: u32, max_h: u32,
            max_v: u32, n_iv: u32, scan_len: u32, gen_ofs: u32,
            mode: u32, ns: u32, ss: u32, se: u32, ah: u32, al: u32,
            total_mcus: u32, scomp: vec4<u32>, tab_ofs: u32, flut_ofs: u32, }

@group(0) @binding(0) var<storage, read> scan: array<u32>;
@group(0) @binding(1) var<storage, read> ck: array<vec4<u32>>;
@group(0) @binding(2) var<storage, read> tab: array<vec4<u32>>;
@group(0) @binding(3) var<storage, read_write> st: array<u32>;
@group(0) @binding(4) var<uniform> comps: Comps;
@group(0) @binding(8) var<storage, read> flut: array<i32>;
@group(0) @binding(5) var<storage, read_write> coef: array<u32>;
var<immediate> pc: Pc;

fn cm(c: u32, i: u32) -> i32 { return comps.c[c][i >> 2u][i & 3u]; }

struct Br {
    pos: u32, buf: u32, cnt: i32, e_end: bool,
    wpos: u32, wval: u32, nval: u32, pad: i32,
}

fn byte_at(p: u32) -> u32 {
    return (scan[p >> 2u] >> ((p & 3u) * 8u)) & 0xFFu;
}

fn rd_byte(b: ptr<function, Br>) -> u32 {
    if (*b).e_end { return 0xFFu; }                 // pad 1s
    let wp = (*b).pos >> 2u;
    if (wp != (*b).wpos) {
        // rotate in prefetched next word, prefetch the following one
        if (wp == (*b).wpos + 1u) { (*b).wval = (*b).nval; }
        else { (*b).wval = scan[wp]; }
        if (wp + 1u < ((pc.scan_len + 3u) >> 2u)) {
            (*b).nval = scan[wp + 1u];
        } else { (*b).nval = 0u; }
        (*b).wpos = wp;
    }
    let v = ((*b).wval >> (((*b).pos & 3u) * 8u)) & 0xFFu;
    if (v != 0xFFu) { (*b).pos += 1u; return v; }
    // 0xFF: crawl over fill-byte run, classify the byte that follows
    var p = (*b).pos + 1u;
    var n = 0xFFu;
    while (n == 0xFFu && p < pc.scan_len) {
        n = byte_at(p);
        if (n == 0xFFu) { p += 1u; }
    }
    if (n == 0u) {
        (*b).pos = p + 1u;
        (*b).wpos = 0xFFFFFFFEu;
        return 0xFFu;
    }
    (*b).e_end = true;                              // marker: stop
    return 0xFFu;
}

fn fill_bits(b: ptr<function, Br>) {
    // pad counts the trailing buffered bits that are marker padding, not
    // real data — an MCU may only be skipped when nothing but pad remains.
    while ((*b).cnt <= 24) {
        let v = rd_byte(b);
        (*b).buf = ((*b).buf << 8u) | v;
        (*b).cnt += 8;
        if ((*b).e_end) { (*b).pad += 8; }
    }
}

fn peek_n(b: ptr<function, Br>, n: u32) -> u32 {
    fill_bits(b);
    if ((*b).cnt >= i32(n)) {
        return ((*b).buf >> (u32((*b).cnt) - n)) & ((1u << n) - 1u);
    }
    let real = u32((*b).cnt);
    let v = ((*b).buf & ((1u << real) - 1u)) << (n - real);
    return v | ((1u << (n - real)) - 1u);
}

fn eat(b: ptr<function, Br>, n: u32) {
    if (u32((*b).cnt) >= n) { (*b).cnt -= i32(n); }
    else { (*b).cnt = 0; }
    if ((*b).pad > (*b).cnt) { (*b).pad = (*b).cnt; }
    if ((*b).cnt < 32) { (*b).buf &= (1u << u32((*b).cnt)) - 1u; }
    // cnt >= 32: buf &= 0xFFFFFFFF is a no-op
}

fn get1(b: ptr<function, Br>) -> u32 {
    let v = peek_n(b, 1u);
    eat(b, 1u);
    return v;
}

fn rext(b: ptr<function, Br>, s: i32) -> i32 {
    if (s == 0) { return 0; }
    let v = peek_n(b, u32(s));
    eat(b, u32(s));
    let i = i32(v);
    let vt = 1 << u32(s - 1);
    if (i < vt) { return i - (1 << u32(s)) + 1; }
    return i;
}

// coef buffer: i16 pairs per u32 word, zigzag order — 32 words/block
fn get_coef(outp: u32, k: u32) -> i32 {
    let w2 = coef[outp + (k >> 1u)];
    var v = i32((w2 >> ((k & 1u) * 16u)) & 0xFFFFu);
    if (v >= 32768) { v -= 65536; }
    return v;
}
fn set_coef(outp: u32, k: u32, v: i32) {
    let wi = outp + (k >> 1u);
    var w2 = coef[wi];
    let sh = (k & 1u) * 16u;
    w2 = (w2 & ~(0xFFFFu << sh)) | ((bitcast<u32>(v) & 0xFFFFu) << sh);
    coef[wi] = w2;
}

// ---- sequential (baseline) block: DC diff + full AC band ----
fn seq_block(b: ptr<function, Br>, outp: u32, dt: u32, at: u32,
             pred: i32) -> i32 {
    // -- DC: fused entry -> sign-extended diff in one lookup
    var e = flut[pc.flut_ofs + dt * 65536u + peek_n(b, 16u)];
    var do_ac = true;
    var diff: i32;
    if (e < 0) {
        eat(b, u32(e & 0x1F));
        diff = ((e >> 13u) & 0xFFFF) - 32768;
    } else {
        let len = e & 0x1F;
        if (len == 0) { do_ac = false; diff = 0; }
        else {
            eat(b, u32(len));
            diff = rext(b, (e >> 8u) & 0xFF);
        }
    }
    let p2 = pred + diff;
    var pw = 0u;
    var pv = bitcast<u32>(p2) & 0xFFFFu;
    // -- AC: ZRL/EOB need no stores; nonzero coefs gather
    //    into the pending word
    if (do_ac) {
        var k = 1;
        while (k < 64) {
            e = flut[pc.flut_ofs + at * 65536u + peek_n(b, 16u)];
            var rr: i32; var ss: i32; var val: i32;
            if (e < 0) {
                eat(b, u32(e & 0x1F));
                rr = (e >> 5u) & 0xF;
                ss = (e >> 9u) & 0xF;
                val = ((e >> 13u) & 0xFFFF) - 32768;
            } else {
                let len = e & 0x1F;
                if (len == 0) { break; }
                eat(b, u32(len));
                let sym = (e >> 8u) & 0xFF;
                rr = sym >> 4u;
                ss = sym & 15;
                if (ss > 0) { val = rext(b, ss); }
                else { val = 0; }
            }
            if (ss == 0) {
                if (rr == 15) { k += 16; continue; }
                break;              // EOB
            }
            k += rr;
            if (k < 64) {
                let wv = u32(k >> 1u);
                if (wv != pw) {
                    coef[outp + pw] = pv;
                    pw = wv;
                    pv = 0u;
                }
                pv |= (bitcast<u32>(val) & 0xFFFFu) <<
                      (u32(k & 1) * 16u);
            }
            k += 1;
        }
    }
    coef[outp + pw] = pv;
    return p2;
}

// ---- progressive modes (jdphuff.c) ----

// DC first: coef[0] = (pred + diff) << Al
fn dc_first(b: ptr<function, Br>, outp: u32, dt: u32, pred: i32) -> i32 {
    let e = flut[pc.flut_ofs + dt * 65536u + peek_n(b, 16u)];
    var diff = 0;
    if (e < 0) {
        eat(b, u32(e & 0x1F));
        diff = ((e >> 13u) & 0xFFFF) - 32768;
    } else {
        let len = e & 0x1F;
        if (len > 0) {
            eat(b, u32(len));
            diff = rext(b, (e >> 8u) & 0xFF);
        }
    }
    let p2 = pred + diff;
    set_coef(outp, 0u, p2 << u32(pc.al));
    return p2;
}

// DC refine: one correction bit per block — coef[0] |= (1 << Al)
fn dc_refine(b: ptr<function, Br>, outp: u32) {
    if (get1(b) != 0u) {
        set_coef(outp, 0u, get_coef(outp, 0u) | (1 << u32(pc.al)));
    }
}

// AC first: band Ss..Se, v << Al writes, EOBRUN zero-runs
fn ac_first(b: ptr<function, Br>, outp: u32, at: u32, eob: u32) -> u32 {
    var eobrun = eob;
    if (eobrun > 0u) { return eobrun - 1u; }   // EOB block: no bits read
    var k = i32(pc.ss);
    while (k <= i32(pc.se)) {
        let e = flut[pc.flut_ofs + at * 65536u + peek_n(b, 16u)];
        var rr: i32; var s: i32; var val: i32;
        if (e < 0) {
            eat(b, u32(e & 0x1F));
            rr = (e >> 5u) & 0xF; s = (e >> 9u) & 0xF;
            val = ((e >> 13u) & 0xFFFF) - 32768;
        } else {
            let len = e & 0x1F;
            if (len == 0) { break; }
            eat(b, u32(len));
            let sym = (e >> 8u) & 0xFF;
            rr = sym >> 4u; s = sym & 15;
            if (s > 0) { val = rext(b, s); } else { val = 0; }
        }
        if (s != 0) {
            k += rr;
            if (k <= i32(pc.se)) {
                set_coef(outp, u32(k), val << u32(pc.al));
            }
            k += 1;
        } else if (rr == 15) {       // ZRL
            k += 16;
        } else {                     // EOBr: 2^r + appended bits
            eobrun = 1u << u32(rr);
            if (rr > 0) {
                eobrun += peek_n(b, u32(rr));
                eat(b, u32(rr));
            }
            eobrun -= 1u;            // this block consumed one
            break;
        }
    }
    return eobrun;
}

// AC refine: correction bits on already-nonzero coefs, new +-p1 coefs
fn ac_refine(b: ptr<function, Br>, outp: u32, at: u32, eob: u32) -> u32 {
    let p1 = 1 << u32(pc.al);
    let m1 = -p1;
    var k = i32(pc.ss);
    var eobrun = eob;
    if (eobrun == 0u) {
        while (k <= i32(pc.se)) {
            let e = flut[pc.flut_ofs + at * 65536u + peek_n(b, 16u)];
            var r: i32; var s: i32; var newv = 0;
            if (e < 0) {
                eat(b, u32(e & 0x1F));
                r = (e >> 5u) & 0xF; s = (e >> 9u) & 0xF;
                if (s != 0) {
                    newv = (((e >> 13u) & 0xFFFF) - 32768) << u32(pc.al);
                }
            } else {
                let len = e & 0x1F;
                if (len == 0) { break; }
                eat(b, u32(len));
                let sym = (e >> 8u) & 0xFF;
                r = sym >> 4u; s = sym & 15;
                if (s != 0) {
                    newv = select(m1, p1, get1(b) != 0u);
                }
            }
            if (s == 0 && r != 15) {   // EOBr: rest of band = corrections
                eobrun = 1u << u32(r);
                if (r > 0) {
                    eobrun += peek_n(b, u32(r));
                    eat(b, u32(r));
                }
                break;
            }
            // walk: correction bits on nonzeros, count down r zeros
            loop {
                let v = get_coef(outp, u32(k));
                if (v != 0) {
                    if (get1(b) != 0u && (v & p1) == 0) {
                        set_coef(outp, u32(k),
                                 v + select(m1, p1, v >= 0));
                    }
                } else {
                    r -= 1;
                    if (r < 0) { break; }   // target zero position
                }
                k += 1;
                if (k > i32(pc.se)) { break; }
            }
            if (s != 0 && k <= i32(pc.se)) {
                set_coef(outp, u32(k), newv);
            }
            k += 1;
        }
    }
    if (eobrun > 0u) {
        // rest of band (or whole block under EOBRUN): correction bits
        while (k <= i32(pc.se)) {
            let v = get_coef(outp, u32(k));
            if (v != 0 && get1(b) != 0u && (v & p1) == 0) {
                set_coef(outp, u32(k), v + select(m1, p1, v >= 0));
            }
            k += 1;
        }
        eobrun -= 1u;
    }
    return eobrun;
}

@compute @workgroup_size(32)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= pc.n_iv) { return; }               // dispatch overrun
    let idx = pc.gen_ofs + gid.x;                   // (iv,chunk) grid
    let w = ck[idx];
    if (w.z == 0u) { return; }                      // padded grid cell
    var b: Br;
    var pred: vec4<i32>;
    var eobrun = 0u;
    if (w.w == 0u) {                                // first chunk
        b.pos = w.x; b.buf = 0u; b.cnt = 0; b.e_end = false;
        b.wpos = 0xFFFFFFFEu; b.wval = 0u; b.nval = 0u; b.pad = 0;
        pred = vec4<i32>(0);
    } else {
        // 1: resume predecessor generation's state (idx - n_iv).
        // 2: host-surveyed virtual-restart state in OUR OWN slot (idx).
        var s = (idx - pc.n_iv) * 12u;
        if (w.w == 2u) { s = idx * 12u; }
        b.pos = st[s]; b.buf = st[s + 1u]; b.cnt = i32(st[s + 2u]);
        b.e_end = st[s + 3u] != 0u;
        b.wpos = 0xFFFFFFFEu; b.wval = 0u; b.nval = 0u;
        b.pad = i32(st[s + 9u]);
        pred = vec4<i32>(bitcast<i32>(st[s + 4u]), bitcast<i32>(st[s + 5u]),
                         bitcast<i32>(st[s + 6u]), bitcast<i32>(st[s + 7u]));
        eobrun = st[s + 8u];
    }

    for (var m = 0u; m < w.z; m += 1u) {
        let mcu = i32(w.y + m);
        if (mcu >= i32(pc.total_mcus)) { break; }   // malformed interval table
        if (b.e_end && b.cnt <= b.pad) { continue; } // no data left: skip MCU
        if (pc.ns > 1u) {                            // interleaved: MCU grid
            let mx = mcu % i32(pc.mcus_x);
            let my = mcu / i32(pc.mcus_x);
            for (var ci = 0; ci < i32(pc.ns); ci += 1) {
                let scw = pc.scomp[ci];
                let comp = scw & 0xFFu;
                let dt = (scw >> 8u) & 0xFFu;
                let at = 4u + ((scw >> 16u) & 0xFFu);
                let hs = cm(comp, 0u);
                let vs = cm(comp, 1u);
                let bw = cm(comp, 2u);
                let cofs = u32(cm(comp, 9u));
                for (var v = 0; v < vs; v += 1) {
                    for (var h = 0; h < hs; h += 1) {
                        let bi = (my * vs + v) * bw + mx * hs + h;
                        let outp = cofs + u32(bi) * 32u;
                        switch pc.mode {
                            case 0u: {
                                pred[ci] = seq_block(&b, outp, dt, at, pred[ci]);
                            }
                            case 1u: {
                                pred[ci] = dc_first(&b, outp, dt, pred[ci]);
                            }
                            case 2u: { dc_refine(&b, outp); }
                            default: {}
                        }
                    }
                }
            }
        } else {                                     // non-interleaved: raster
            let scw = pc.scomp[0];
            let comp = scw & 0xFFu;
            let dt = (scw >> 8u) & 0xFFu;
            let at = 4u + ((scw >> 16u) & 0xFFu);
            let wib1 = cm(comp, 11u);
            let bw = cm(comp, 2u);
            let cofs = u32(cm(comp, 9u));
            let bx = mcu % wib1;
            let by = mcu / wib1;
            let outp = cofs + u32(by * bw + bx) * 32u;
            switch pc.mode {
                case 0u: {
                    pred[0] = seq_block(&b, outp, dt, at, pred[0]);
                }
                case 1u: {
                    pred[0] = dc_first(&b, outp, dt, pred[0]);
                }
                case 2u: { dc_refine(&b, outp); }
                case 3u: { eobrun = ac_first(&b, outp, at, eobrun); }
                default: { eobrun = ac_refine(&b, outp, at, eobrun); }
            }
        }
    }

    // hand bitstate + DC predictors + EOBRUN to the next generation's chunk
    let s = idx * 12u;
    st[s] = b.pos; st[s + 1u] = b.buf; st[s + 2u] = u32(b.cnt);
    st[s + 3u] = select(0u, 1u, b.e_end);
    st[s + 4u] = bitcast<u32>(pred[0]); st[s + 5u] = bitcast<u32>(pred[1]);
    st[s + 6u] = bitcast<u32>(pred[2]); st[s + 7u] = bitcast<u32>(pred[3]);
    st[s + 8u] = eobrun; st[s + 9u] = u32(b.pad);
}
"#;

pub const RECON: &str = r#"
struct Comps { c: array<array<vec4<i32>, 4>, 4>, }
struct Pc8 { w: u32, h: u32, mcus_x: u32, n_comp: u32, max_h: u32,
             max_v: u32, n_iv: u32, scan_len: u32, tab_ofs: u32,
             orient: u32, uW: u32, uH: u32, }

@group(0) @binding(2) var<storage, read> tab: array<vec4<u32>>;
@group(0) @binding(4) var<uniform> comps: Comps;
@group(0) @binding(5) var<storage, read_write> coef: array<u32>;
@group(0) @binding(6) var<storage, read_write> planes: array<u32>;
var<immediate> pc: Pc8;

fn t_at(i: u32) -> i32 {
    return bitcast<i32>(tab[pc.tab_ofs + (i >> 2u)][i & 3u]);
}
fn cm(c: u32, i: u32) -> i32 { return comps.c[c][i >> 2u][i & 3u]; }

const ZIG = array<i32, 64>(
    0,1,8,16,9,2,3,10,17,24,32,25,18,11,4,5,
    12,19,26,33,40,48,41,34,27,20,13,6,7,14,21,28,
    35,42,49,56,57,50,43,36,29,22,15,23,30,37,44,51,
    58,59,52,45,38,31,39,46,53,60,61,54,47,55,62,63);

fn desc(x: i32, n: i32) -> i32 { return (x + (1 << u32(n - 1))) >> u32(n); }

// one full islow pass over 8 values (shared by both directions)
fn idct1d(inp: ptr<function, array<i32, 8>>,
          outp: ptr<function, array<i32, 8>>, sc: i32, nsh: i32) {
    var z2 = (*inp)[2] * sc;
    var z3 = (*inp)[6] * sc;
    var z1 = (z2 + z3) * 4433;                     // FIX_0_541196100
    var t2 = z1 - z3 * 15137;                      // FIX_1_847759065
    var t3 = z1 + z2 * 6270;                       // FIX_0_765366865
    z2 = (*inp)[0] * sc;
    z3 = (*inp)[4] * sc;
    var t0 = (z2 + z3) << 13u;
    var t1 = (z2 - z3) << 13u;
    let t10 = t0 + t3;
    let t13 = t0 - t3;
    let t11 = t1 + t2;
    let t12 = t1 - t2;
    t0 = (*inp)[7] * sc;
    t1 = (*inp)[5] * sc;
    t2 = (*inp)[3] * sc;
    t3 = (*inp)[1] * sc;
    z1 = t0 + t3;
    z2 = t1 + t2;
    let z3o = t0 + t2;
    var z4 = t1 + t3;
    let z5 = (z3o + z4) * 9633;                    // FIX_1_175875602
    t0 *= 2446;
    t1 *= 16819;
    t2 *= 25172;
    t3 *= 12299;
    z1 *= -7373;
    z2 *= -20995;
    var z3o2 = z3o * -16069;
    z4 *= -3196;
    z3o2 += z5;
    z4 += z5;
    t0 += z1 + z3o2;
    t1 += z2 + z4;
    t2 += z2 + z3o2;
    t3 += z1 + z4;
    (*outp)[0] = desc(t10 + t3, nsh);
    (*outp)[7] = desc(t10 - t3, nsh);
    (*outp)[1] = desc(t11 + t2, nsh);
    (*outp)[6] = desc(t11 - t2, nsh);
    (*outp)[2] = desc(t12 + t1, nsh);
    (*outp)[5] = desc(t12 - t1, nsh);
    (*outp)[3] = desc(t13 + t0, nsh);
    (*outp)[4] = desc(t13 - t0, nsh);
}

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid3: vec3<u32>) {
    var gid = gid3.x;
    // find which component this block belongs to
    var c = 0;
    while (c < i32(pc.n_comp) - 1 &&
           gid >= u32(cm(u32(c), 2u) * cm(u32(c), 3u))) {
        gid -= u32(cm(u32(c), 2u) * cm(u32(c), 3u));
        c += 1;
    }
    if (c >= i32(pc.n_comp) ||
        i32(gid) >= cm(u32(c), 2u) * cm(u32(c), 3u)) {
        return;
    }
    let bw = cm(u32(c), 2u);
    let bx = i32(gid) % bw;
    let by = i32(gid) / bw;
    let qt = cm(u32(c), 6u);
    let cofs = u32(cm(u32(c), 9u));
    let pofs = u32(cm(u32(c), 10u)) >> 2u;          // bytes -> words

    // dequantize (zigzag buffer -> natural order)
    var deq: array<i32, 64>;
    for (var i = 0; i < 64; i += 2) {
        let w2 = coef[cofs + u32(bx + by * bw) * 32u + u32(i / 2)];
        var v0 = i32(w2 & 0xFFFFu);
        if (v0 >= 32768) { v0 -= 65536; }
        var v1 = i32(w2 >> 16u);
        if (v1 >= 32768) { v1 -= 65536; }
        deq[ZIG[i]] = v0 * t_at(u32(qt * 64 + ZIG[i]));
        deq[ZIG[i + 1]] = v1 * t_at(u32(qt * 64 + ZIG[i + 1]));
    }

    // pass 1: columns
    var ws: array<i32, 64>;
    for (var col = 0; col < 8; col += 1) {
        var inp: array<i32, 8>;
        var outp: array<i32, 8>;
        for (var r = 0; r < 8; r += 1) { inp[r] = deq[r * 8 + col]; }
        idct1d(&inp, &outp, 1, 11);                // scale 1, descale 11
        for (var r = 0; r < 8; r += 1) { ws[r * 8 + col] = outp[r]; }
    }
    // pass 2: rows -> u8, pack 4 per word
    for (var r = 0; r < 8; r += 1) {
        var inp: array<i32, 8>;
        var outp: array<i32, 8>;
        for (var cc = 0; cc < 8; cc += 1) { inp[cc] = ws[r * 8 + cc]; }
        idct1d(&inp, &outp, 1, 18);                // descale 18
        let base = pofs + u32(by * 8 + r) * u32(bw * 2) + u32(bx * 2);
        var w0 = 0u;
        var w1 = 0u;
        for (var cc = 0; cc < 4; cc += 1) {
            let a = clamp(outp[cc] + 128, 0, 255);
            let b = clamp(outp[cc + 4] + 128, 0, 255);
            w0 |= u32(a) << (u32(cc) * 8u);
            w1 |= u32(b) << (u32(cc) * 8u);
        }
        planes[base] = w0;
        planes[base + 1u] = w1;
    }
}
"#;

pub const UPSAMPLE: &str = r#"
struct Comps { c: array<array<vec4<i32>, 4>, 4>, }
struct Pc8 { w: u32, h: u32, mcus_x: u32, n_comp: u32, max_h: u32,
             max_v: u32, n_iv: u32, scan_len: u32, tab_ofs: u32,
             orient: u32, uW: u32, uH: u32, }

@group(0) @binding(4) var<uniform> comps: Comps;
@group(0) @binding(6) var<storage, read_write> planes: array<u32>;
@group(0) @binding(7) var<storage, read_write> outp: array<u32>;
var<immediate> pc: Pc8;

fn cm(c: u32, i: u32) -> i32 { return comps.c[c][i >> 2u][i & 3u]; }

// fetch one u8 sample from component c's packed plane
fn smp(c: u32, x: i32, y: i32) -> i32 {
    let pofs = u32(cm(c, 10u));
    let stride = cm(c, 2u) * 8;                    // samples per plane row
    let a = pofs + u32(y * stride + x);
    return i32((planes[a >> 2u] >> ((a & 3u) * 8u)) & 0xFFu);
}

// upsampled component value at output pixel (x,y)
fn usamp(c: u32, x: i32, y: i32) -> i32 {
    let hs = cm(c, 0u);
    let vs = cm(c, 1u);
    let cw = cm(c, 4u);
    let ch = cm(c, 5u);
    let rh = i32(pc.max_h) / hs;
    let rv = i32(pc.max_v) / vs;
    if (rh == 1 && rv == 1) { return smp(c, x, y); }
    if (rh == 2 && rv == 1) {                      // h2v1 fancy
        let j = clamp(x >> 1u, 0, cw - 1);
        if (x == 0) { return smp(c, 0, y); }
        if (x == cw * 2 - 1) { return smp(c, cw - 1, y); }
        if ((x & 1) == 0) {
            return (3 * smp(c, j, y) + smp(c, j - 1, y) + 1) >> 2u;
        }
        return (3 * smp(c, j, y) + smp(c, j + 1, y) + 2) >> 2u;
    }
    if (rh == 1 && rv == 2) {                      // h1v2 fancy
        let i = clamp(y >> 1u, 0, ch - 1);
        let adj = clamp(i + select(1, -1, (y & 1) == 0), 0, ch - 1);
        let bias = select(2, 1, (y & 1) == 0);
        return (3 * smp(c, x, i) + smp(c, x, adj) + bias) >> 2u;
    }
    if (rh == 2 && rv == 2 && cw > 2) {            // h2v2 fancy
        let i = clamp(y >> 1u, 0, ch - 1);
        let i2 = clamp(i + select(1, -1, (y & 1) == 0), 0, ch - 1);
        let j = clamp(x >> 1u, 0, cw - 1);
        let this0 = 3 * smp(c, j, i) + smp(c, j, i2);   // colsum j
        if (x == 0) { return (this0 * 4 + 8) >> 4u; }
        if (x == cw * 2 - 1) { return (this0 * 4 + 7) >> 4u; }
        let j2 = clamp(j + select(1, -1, (x & 1) == 0), 0, cw - 1);
        let adj0 = 3 * smp(c, j2, i) + smp(c, j2, i2);
        let bias = select(7, 8, (x & 1) == 0);
        return (this0 * 3 + adj0 + bias) >> 4u;
    }
    // integral factors: plain replication
    let cx = clamp(x / rh, 0, cw - 1);
    let cy = clamp(y / rv, 0, ch - 1);
    return smp(c, cx, cy);
}

// EXIF orientation: output (x,y) in oriented space -> source coords
// in the un-oriented uW x uH plane layout.
fn omap(x: i32, y: i32) -> vec2<i32> {
    let uw = i32(pc.uW);
    let uh = i32(pc.uH);
    switch (pc.orient) {
    case 2u:  { return vec2<i32>(uw - 1 - x, y); }
    case 3u:  { return vec2<i32>(uw - 1 - x, uh - 1 - y); }
    case 4u:  { return vec2<i32>(x, uh - 1 - y); }
    case 5u:  { return vec2<i32>(y, x); }
    case 6u:  { return vec2<i32>(y, uh - 1 - x); }
    case 7u:  { return vec2<i32>(uw - 1 - y, uh - 1 - x); }
    case 8u:  { return vec2<i32>(uw - 1 - y, x); }
    default:  { return vec2<i32>(x, y); }
    }
}

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let x = gid.x;
    let y = gid.y;
    if (x >= pc.w || y >= pc.h) { return; }
    let uv = omap(i32(x), i32(y));
    var r: i32; var g: i32; var bl: i32;
    if (pc.n_comp == 1u) {
        let v = usamp(0u, uv.x, uv.y);
        r = v; g = v; bl = v;
    } else {
        let yy = usamp(0u, uv.x, uv.y);
        let cb = usamp(1u, uv.x, uv.y) - 128;
        let cr = usamp(2u, uv.x, uv.y) - 128;
        r = clamp(yy + ((91881 * cr + 32768) >> 16u), 0, 255);
        g = clamp(yy + ((32768 - 22554 * cb - 46802 * cr) >> 16u), 0, 255);
        bl = clamp(yy + ((116130 * cb + 32768) >> 16u), 0, 255);
    }
    outp[y * pc.w + x] = 0xFF000000u | (u32(bl) << 16u) |
                         (u32(g) << 8u) | u32(r);
}
"#;

/// Resize pipeline: 4 bindings (in, bounds, coefs, out), ≤24B push.
pub const RESAMPLE: &str = r#"
struct Pc { in_w: u32, in_h: u32, out_w: u32, out_h: u32,
            ksize: u32, axis: u32, }

@group(0) @binding(0) var<storage, read> in_buf: array<u32>;
@group(0) @binding(1) var<storage, read> bnd: array<vec2<i32>>;
@group(0) @binding(2) var<storage, read> cf: array<i32>;
@group(0) @binding(3) var<storage, read_write> out_buf: array<u32>;
var<immediate> pc: Pc;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let ox = gid.x;
    let oy = gid.y;
    if (ox >= pc.out_w || oy >= pc.out_h) { return; }
    let o = select(oy, ox, pc.axis == 0u);
    let b = bnd[o];
    // ss = 1 << (PRECISION_BITS-1) per channel
    var ss = vec4<i32>(1 << 21u);
    for (var i = 0; i < b.y; i += 1) {
        let k = cf[o * u32(pc.ksize) + u32(i)];
        let src = u32(b.x + i);
        var px: u32;
        if (pc.axis == 0u) { px = in_buf[oy * pc.in_w + src]; }
        else { px = in_buf[src * pc.in_w + ox]; }
        ss += vec4<i32>(i32(px & 0xFFu), i32((px >> 8u) & 0xFFu),
                        i32((px >> 16u) & 0xFFu), 0) * k;
    }
    let r = u32(clamp(ss.r >> 22u, 0, 255));
    let g = u32(clamp(ss.g >> 22u, 0, 255));
    let bch = u32(clamp(ss.b >> 22u, 0, 255));
    out_buf[oy * pc.out_w + ox] = r | (g << 8u) | (bch << 16u) | 0xFF000000u;
}
"#;

/// Baseline JPEG encoder — one invocation per MCU, restart-parallel
/// (DRI=1, host wraps segments with RST markers). Port of the
/// VulkanExamples/VulcanoCam GLSL encoder; input is the decoder's
/// packed RGBA-word buffer (0xAABBGGRR).
/// Bindings: 0=src rgba words, 1=out (sizes[total] + segments), 2=tab
/// (qY,qC,dcY,acY,dcC,acC packed codes — see gpu.rs).
pub const ENCODE: &str = r#"
struct Pc { w: u32, h: u32, mcus_x: u32, mcus_y: u32,
            seg_words: u32, total: u32, gray: u32, }

@group(0) @binding(0) var<storage, read> src: array<u32>;
@group(0) @binding(1) var<storage, read_write> outp: array<u32>;
@group(0) @binding(2) var<storage, read> tab: array<u32>;
var<immediate> pc: Pc;

const ZZ = array<i32, 64>(
    0,1,8,16,9,2,3,10,17,24,32,25,18,11,4,5,12,19,26,33,40,48,41,34,
    27,20,13,6,7,14,21,28,35,42,49,56,57,50,43,36,29,22,15,23,30,37,
    44,51,58,59,52,45,38,31,39,46,53,60,61,54,47,55,62,63);
const CST = array<f32, 64>(
    1.0000000000e+00,1.0000000000e+00,1.0000000000e+00,1.0000000000e+00,
    1.0000000000e+00,1.0000000000e+00,1.0000000000e+00,1.0000000000e+00,
    9.8078528040e-01,8.3146961230e-01,5.5557023302e-01,1.9509032202e-01,
    -1.9509032202e-01,-5.5557023302e-01,-8.3146961230e-01,-9.8078528040e-01,
    9.2387953251e-01,3.8268343237e-01,-3.8268343237e-01,-9.2387953251e-01,
    -9.2387953251e-01,-3.8268343237e-01,3.8268343237e-01,9.2387953251e-01,
    8.3146961230e-01,-1.9509032202e-01,-9.8078528040e-01,-5.5557023302e-01,
    5.5557023302e-01,9.8078528040e-01,1.9509032202e-01,-8.3146961230e-01,
    7.0710678119e-01,-7.0710678119e-01,-7.0710678119e-01,7.0710678119e-01,
    7.0710678119e-01,-7.0710678119e-01,-7.0710678119e-01,7.0710678119e-01,
    5.5557023302e-01,-9.8078528040e-01,1.9509032202e-01,8.3146961230e-01,
    -8.3146961230e-01,-1.9509032202e-01,9.8078528040e-01,-5.5557023302e-01,
    3.8268343237e-01,-9.2387953251e-01,9.2387953251e-01,-3.8268343237e-01,
    -3.8268343237e-01,9.2387953251e-01,-9.2387953251e-01,3.8268343237e-01,
    1.9509032202e-01,-5.5557023302e-01,8.3146961230e-01,-9.8078528040e-01,
    9.8078528040e-01,-8.3146961230e-01,5.5557023302e-01,-1.9509032202e-01);

fn px(x: i32, y: i32) -> vec3<f32> {
    let cx = clamp(x, 0, i32(pc.w) - 1);
    let cy = clamp(y, 0, i32(pc.h) - 1);
    let v = src[u32(cy) * pc.w + u32(cx)];
    return vec3<f32>(f32(v & 0xFFu) / 255.0,
                     f32((v >> 8u) & 0xFFu) / 255.0,
                     f32((v >> 16u) & 0xFFu) / 255.0);
}

// full-range BT.601 levels, level-shifted for the DCT
fn fetchY(x: i32, y: i32) -> f32 {
    let c = px(x, y);
    return dot(c, vec3<f32>(0.299, 0.587, 0.114)) * 255.0 - 128.0;
}
fn fetchC(x: i32, y: i32) -> vec2<f32> {
    var acc = vec3<f32>(0.0);
    for (var dy = 0; dy < 2; dy += 1) {
        for (var dx = 0; dx < 2; dx += 1) {
            acc += px(x * 2 + dx, y * 2 + dy);
        }
    }
    acc *= 0.25;
    return vec2<f32>(
        (-0.168736 * acc.r - 0.331264 * acc.g + 0.5 * acc.b + 0.5) * 255.0 - 128.0,
        (0.5 * acc.r - 0.418688 * acc.g - 0.081312 * acc.b + 0.5) * 255.0 - 128.0);
}

// ---- bitstream writer (MSB-first, 0xFF byte stuffing) ----
var<private> bit_buf: u32; var<private> word_acc: u32;
var<private> seg_base: u32; var<private> byte_cnt: u32;
var<private> out_word: u32; var<private> bit_len: i32;
var<private> word_bytes: u32;

fn putb(b: u32) {
    word_acc |= b << (word_bytes * 8u);
    word_bytes += 1u;
    byte_cnt += 1u;
    if (word_bytes == 4u) {
        if (out_word < pc.seg_words) { outp[seg_base + out_word] = word_acc; }
        out_word += 1u;
        word_acc = 0u;
        word_bytes = 0u;
    }
}
fn emit_bits(code: u32, len: i32) {
    bit_buf = (bit_buf << u32(len)) | code;
    bit_len += len;
    while (bit_len >= 8) {
        let b = (bit_buf >> u32(bit_len - 8)) & 0xFFu;
        bit_len -= 8;
        bit_buf &= (1u << u32(bit_len)) - 1u;
        putb(b);
        if (b == 0xFFu) { putb(0u); }
    }
}
fn emit_h(h: u32) { emit_bits(h & 0xFFFFu, i32(h >> 16u)); }

fn cat_of(v: i32) -> i32 {
    var a = abs(v); var s = 0;
    while (a != 0) { s += 1; a >>= 1; }
    return s;
}
fn low_bits(v: i32, s: i32) -> u32 {
    if (s <= 0) { return 0u; }
    return u32(select(v, v - 1, v < 0)) & ((1u << u32(s)) - 1u);
}

// float DCT-II via separable row/col passes + quantization
// (same 0.25 * CuCv scale convention as the GLSL encoder)
fn fdctq(f: ptr<function, array<f32, 64>>,
         q: ptr<function, array<i32, 64>>, chroma: bool) {
    var tmp: array<f32, 64>;
    for (var y = 0; y < 8; y += 1) {
        for (var u = 0; u < 8; u += 1) {
            var s = 0.0;
            for (var x = 0; x < 8; x += 1) {
                s += (*f)[y * 8 + x] * CST[u * 8 + x];
            }
            tmp[y * 8 + u] = s;
        }
    }
    for (var v = 0; v < 8; v += 1) {
        for (var u = 0; u < 8; u += 1) {
            var s = 0.0;
            for (var y = 0; y < 8; y += 1) {
                s += tmp[y * 8 + u] * CST[v * 8 + y];
            }
            let c = select(1.0, 0.7071067811865476, u == 0) *
                    select(1.0, 0.7071067811865476, v == 0);
            let qt = select(tab[u32(v * 8 + u)], tab[u32(64 + v * 8 + u)],
                            chroma);
            (*q)[v * 8 + u] = i32(round(0.25 * c * s / f32(qt)));
        }
    }
}

fn enc_block(q: ptr<function, array<i32, 64>>, pred: ptr<function, i32>,
             chroma: bool) {
    let diff = (*q)[0] - *pred;
    *pred = (*q)[0];
    let s = cat_of(diff);
    emit_h(select(tab[128u + u32(s)], tab[400u + u32(s)], chroma));
    emit_bits(low_bits(diff, s), s);
    var run = 0;
    for (var k = 1; k < 64; k += 1) {
        let v = (*q)[ZZ[k]];
        if (v == 0) { run += 1; continue; }
        while (run > 15) {
            emit_h(select(tab[144u + 240u], tab[416u + 240u], chroma));
            run -= 16;
        }
        let sz = cat_of(v);
        emit_h(select(tab[144u + u32((run << 4) | sz)],
                      tab[416u + u32((run << 4) | sz)], chroma));
        emit_bits(low_bits(v, sz), sz);
        run = 0;
    }
    if (run > 0) {
        emit_h(select(tab[144u], tab[416u], chroma));
    }
}

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let mcu = gid.x;
    if (mcu >= pc.total) { return; }
    seg_base = pc.total + mcu * pc.seg_words;
    bit_buf = 0u; bit_len = 0; word_acc = 0u; word_bytes = 0u;
    byte_cnt = 0u; out_word = 0u;
    let mx = i32(mcu % pc.mcus_x);
    let my = i32(mcu / pc.mcus_x);
    var pred_y = 0; var pred_cb = 0; var pred_cr = 0;

    if (pc.gray == 1u) {                  // gray8: one 8x8 block per MCU
        var f: array<f32, 64>;
        for (var y = 0; y < 8; y += 1) {
            for (var x = 0; x < 8; x += 1) {
                f[y * 8 + x] = fetchY(mx * 8 + x, my * 8 + y);
            }
        }
        var q: array<i32, 64>;
        fdctq(&f, &q, false);
        enc_block(&q, &pred_y, false);
    } else {                              // 4:2:0: 4 luma + Cb + Cr
        for (var b = 0; b < 4; b += 1) {
            let bx = mx * 16 + (b & 1) * 8;
            let by = my * 16 + (b >> 1) * 8;
            var f: array<f32, 64>;
            for (var y = 0; y < 8; y += 1) {
                for (var x = 0; x < 8; x += 1) {
                    f[y * 8 + x] = fetchY(bx + x, by + y);
                }
            }
            var q: array<i32, 64>;
            fdctq(&f, &q, false);
            enc_block(&q, &pred_y, false);
        }
        for (var c = 0; c < 2; c += 1) {
            var f: array<f32, 64>;
            for (var y = 0; y < 8; y += 1) {
                for (var x = 0; x < 8; x += 1) {
                    let cc = fetchC(mx * 8 + x, my * 8 + y);
                    f[y * 8 + x] = select(cc.x, cc.y, c == 1);
                }
            }
            var q: array<i32, 64>;
            fdctq(&f, &q, true);
            if (c == 0) { enc_block(&q, &pred_cb, true); }
            else { enc_block(&q, &pred_cr, true); }
        }
    }
    if (bit_len > 0) {
        emit_bits((1u << u32(8 - bit_len)) - 1u, 8 - bit_len);
    }
    if (word_bytes > 0u) {
        if (out_word < pc.seg_words) { outp[seg_base + out_word] = word_acc; }
    }
    outp[mcu] = byte_cnt;
}
"#;

pub const PACK: &str = r#"
struct Pc { npx: u32, n_comp: u32, }

@group(0) @binding(0) var<storage, read> in_buf: array<u32>;
@group(0) @binding(3) var<storage, read_write> out_buf: array<u32>;
var<immediate> pc: Pc;

// 2D-capable dispatch: wgpu caps workgroups per dimension at 65535, so
// huge identity pack-outs spread over y.
@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) nwg: vec3<u32>) {
    let w = gid.x + gid.y * nwg.x * 256u;
    if (w * 4u >= pc.npx * pc.n_comp) { return; }
    var word = 0u;
    for (var k = 0u; k < 4u; k += 1u) {
        let bpos = w * 4u + k;
        if (bpos >= pc.npx * pc.n_comp) { break; }
        let px = bpos / pc.n_comp;
        let ch = bpos - px * pc.n_comp;
        word |= ((in_buf[px] >> (ch * 8u)) & 0xFFu) << (k * 8u);
    }
    out_buf[w] = word;
}
"#;
