//! GPU-accelerated baseline JPEG decode + resize — pure Rust/wgpu.
//!
//! The decoder (ported from the former C++/Vulkan implementation)
//! parallelizes over JPEG restart intervals: the scan is decoded on
//! the GPU once into a full-resolution RGBA buffer kept device-side.
//! `GpuJpeg::resize_*` then runs the PIL-exact separable resampler and
//! the RGB/RGBA pack pass on the GPU, so only the small resized image
//! ever crosses back to the CPU.
//!
//! `IC_NO_GPUJPEG=1` disables it.

mod coeffs;
mod gpu;
mod jpeg;
mod shaders;

use crate::image_utils::fit_dims;
use image::DynamicImage;
use jpeg::{build_canon, pack_comps, parse_jpg, Jpg};
use std::sync::Mutex;

/// Serializes GPU work (queue submissions are internally synchronized
/// in wgpu, but this keeps decode+resize batches atomic and bounds
/// device memory pressure like the C++ version did).
pub static GPU_LOCK: Mutex<()> = Mutex::new(());

const FILTER_LANCZOS: i32 = coeffs::LANCZOS;

#[inline]
fn disabled() -> bool {
    static D: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *D.get_or_init(|| std::env::var("IC_NO_GPUJPEG").is_ok())
}

/// wgpu device + pipelines are usable (adapter found, push constants
/// supported, WGSL compiled). Lazily initializes on first call.
pub fn available() -> bool {
    !disabled() && gpu::ctx().is_some()
}

/// Returns (width, height, dri) if bytes parse as a baseline JPEG we can
/// handle, else None. Pure CPU — never touches the GPU.
pub fn probe(bytes: &[u8]) -> Option<(u32, u32, u32)> {
    let mut j = Jpg::new(bytes);
    if !parse_jpg(&mut j) {
        return None;
    }
    Some((j.w as u32, j.h as u32, j.dri as u32))
}

fn canon(j: &mut Jpg<'_>) {
    for i in 0..4 {
        build_canon(&mut j.dctab[i]);
        build_canon(&mut j.actab[i]);
    }
}

fn open_locked(bytes: &[u8]) -> Option<(GpuJpeg, &'static gpu::GpuCtx)> {
    if disabled() {
        return None;
    }
    let (w, h, dri) = probe(bytes)?;
    if dri == 0 {
        return None; // serial entropy stream — zune wins on CPU
    }
    let ctx = gpu::ctx()?;
    let mut j = Jpg::new(bytes);
    if !parse_jpg(&mut j) {
        return None;
    }
    canon(&mut j);
    let fg = pack_comps(&j);
    let px = ctx.decode_into(&j, &fg);
    Some((
        GpuJpeg {
            px,
            width: w,
            height: h,
        },
        ctx,
    ))
}

/// Full decode -> RGBA (0xAABBGGRR word order, little-endian
/// R,G,B,0xFF per pixel). Falls back to None on anything unsupported.
pub fn decode_rgba(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let _guard = GPU_LOCK.lock().ok()?;
    let (sj, ctx) = open_locked(bytes)?;
    let size = u64::from(sj.width) * u64::from(sj.height) * 4;
    let rgba = ctx.read_buf(&sj.px, size);
    Some((rgba, sj.width, sj.height))
}

/// GPU-resident decoded JPEG. Dropping frees the device buffer.
pub struct GpuJpeg {
    px: wgpu::Buffer,
    pub width: u32,
    pub height: u32,
}

/// Open a baseline JPEG into a GPU-resident RGBA buffer. Returns None
/// for unsupported JPEGs (progressive, non-8-bit, DRI=0) or when no
/// suitable GPU adapter exists.
pub fn open(bytes: &[u8]) -> Option<GpuJpeg> {
    let _guard = GPU_LOCK.lock().ok()?;
    open_locked(bytes).map(|(j, _)| j)
}

impl GpuJpeg {
    fn resize_packed(&self, max_w: u32, max_h: u32, n_comp: u32) -> Option<(Vec<u8>, u32, u32)> {
        let (ow, oh) = fit_dims(self.width, self.height, max_w, max_h);
        let ctx = gpu::ctx()?;
        let _guard = GPU_LOCK.lock().ok()?;
        let out = ctx.resize_pack(
            &self.px,
            self.width,
            self.height,
            ow,
            oh,
            n_comp,
            FILTER_LANCZOS,
        )?;
        Some((out, ow, oh))
    }

    /// Resize-to-fit on GPU, returning packed RGB bytes.
    pub fn resize_rgb(&self, max_w: u32, max_h: u32) -> Option<(Vec<u8>, u32, u32)> {
        self.resize_packed(max_w, max_h, 3)
    }

    /// Resize-to-fit on GPU, returning packed RGBA bytes.
    pub fn resize_rgba(&self, max_w: u32, max_h: u32) -> Option<(Vec<u8>, u32, u32)> {
        self.resize_packed(max_w, max_h, 4)
    }

    pub fn resize_image(&self, max_w: u32, max_h: u32) -> Option<DynamicImage> {
        let (rgb, ow, oh) = self.resize_rgb(max_w, max_h)?;
        image::RgbImage::from_raw(ow, oh, rgb).map(DynamicImage::ImageRgb8)
    }

    pub fn resize_image_rgba(&self, max_w: u32, max_h: u32) -> Option<DynamicImage> {
        let (rgba, ow, oh) = self.resize_rgba(max_w, max_h)?;
        image::RgbaImage::from_raw(ow, oh, rgba).map(DynamicImage::ImageRgba8)
    }
}
