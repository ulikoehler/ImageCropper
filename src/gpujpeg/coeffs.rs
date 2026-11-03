//! PIL-exact resample coefficients — pure-Rust port of the host side of
//! VulkanExamples/p81_resize (itself a port of libImaging/Resample.c).
//! 22-bit fixed-point quantization, round-half-away-from-zero.

type FilterFn = fn(f64) -> f64;
pub struct FilterDef {
    pub f: FilterFn,
    pub support: f64,
}

fn box_filter(x: f64) -> f64 {
    if x > -0.5 && x <= 0.5 {
        1.0
    } else {
        0.0
    }
}
fn bilinear_filter(x: f64) -> f64 {
    let x = x.abs();
    if x < 1.0 {
        1.0 - x
    } else {
        0.0
    }
}
fn hamming_filter(x: f64) -> f64 {
    let x = x.abs();
    if x == 0.0 {
        return 1.0;
    }
    if x >= 1.0 {
        return 0.0;
    }
    let x = x * std::f64::consts::PI;
    x.sin() / x * (0.54 + 0.46 * x.cos())
}
fn bicubic_filter(x: f64) -> f64 {
    const A: f64 = -0.5;
    let x = x.abs();
    if x < 1.0 {
        return ((A + 2.0) * x - (A + 3.0)) * x * x + 1.0;
    }
    if x < 2.0 {
        return (((x - 5.0) * x + 8.0) * x - 4.0) * A;
    }
    0.0
}
fn sinc_filter(x: f64) -> f64 {
    if x == 0.0 {
        return 1.0;
    }
    let x = x * std::f64::consts::PI;
    x.sin() / x
}
fn lanczos_filter(x: f64) -> f64 {
    if (-3.0..3.0).contains(&x) {
        return sinc_filter(x) * sinc_filter(x / 3.0);
    }
    0.0
}

/// filter id: 0=nearest 1=box 2=bilinear 3=hamming 4=bicubic 5=lanczos
pub const FILTER_DEFS: [FilterDef; 5] = [
    FilterDef {
        f: box_filter,
        support: 0.5,
    },
    FilterDef {
        f: bilinear_filter,
        support: 1.0,
    },
    FilterDef {
        f: hamming_filter,
        support: 1.0,
    },
    FilterDef {
        f: bicubic_filter,
        support: 2.0,
    },
    FilterDef {
        f: lanczos_filter,
        support: 3.0,
    },
];

pub const LANCZOS: i32 = 5;

const PREC_BITS: i32 = 22;

pub struct Axis {
    pub ksize: i32,
    pub bounds: Vec<i32>, // 2 per out px: {xmin, xmax}
    pub coefs: Vec<i32>,  // outSize * ksize
}

fn precompute_coeffs(in_size: i32, out_size: i32, f: &FilterDef) -> Axis {
    let scale = f64::from(in_size) / f64::from(out_size);
    let filterscale = scale.max(1.0);
    let support = f.support * filterscale;
    let ksize = support.ceil() as i32 * 2 + 1;
    let inv_fs = 1.0 / filterscale;

    let mut a = Axis {
        ksize,
        bounds: vec![0; (out_size * 2) as usize],
        coefs: vec![0; (out_size * ksize) as usize],
    };
    let mut k = vec![0.0f64; ksize as usize];
    for xx in 0..out_size {
        let center = (f64::from(xx) + 0.5) * scale;
        let mut xmin = (center - support + 0.5) as i32;
        if xmin < 0 {
            xmin = 0;
        }
        let mut xmax = (center + support + 0.5) as i32;
        if xmax > in_size {
            xmax = in_size;
        }
        xmax -= xmin;
        let mut ww = 0.0;
        for x in 0..xmax {
            let w = (f.f)((f64::from(x + xmin) - center + 0.5) * inv_fs);
            k[x as usize] = w;
            ww += w;
        }
        if ww != 0.0 {
            for x in 0..xmax as usize {
                k[x] /= ww;
            }
        }
        for x in xmax as usize..ksize as usize {
            k[x] = 0.0;
        }
        for x in 0..ksize as usize {
            let v = k[x] * f64::from(1 << PREC_BITS);
            a.coefs[(xx * ksize) as usize + x] = if v < 0.0 { v - 0.5 } else { v + 0.5 } as i32;
        }
        a.bounds[(xx * 2) as usize] = xmin;
        a.bounds[(xx * 2 + 1) as usize] = xmax;
    }
    a
}

// PIL NEAREST accumulates xo += a0 iteratively in double (see
// p81_resize); expressed as a degenerate 1-tap resample so the same
// shader path reproduces it.
fn nearest_coeffs(in_size: i32, out_size: i32) -> Axis {
    let mut a = Axis {
        ksize: 1,
        bounds: vec![0; (out_size * 2) as usize],
        coefs: vec![0; out_size as usize],
    };
    let a0 = f64::from(in_size) / f64::from(out_size);
    let mut xo = 0.5 * a0;
    for xx in 0..out_size {
        let mut sx = xo as i32;
        if sx >= in_size {
            sx = in_size - 1;
        }
        a.bounds[(xx * 2) as usize] = sx;
        a.bounds[(xx * 2 + 1) as usize] = 1;
        a.coefs[xx as usize] = 1 << PREC_BITS;
        xo += a0;
    }
    a
}

pub fn axis_coeffs(in_size: i32, out_size: i32, filter: i32) -> Axis {
    if filter == 0 {
        nearest_coeffs(in_size, out_size)
    } else {
        precompute_coeffs(in_size, out_size, &FILTER_DEFS[(filter - 1) as usize])
    }
}
