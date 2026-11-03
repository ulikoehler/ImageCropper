//! GPU-accelerated JPEG decode + resize — pure Rust/wgpu.
//!
//! The decoder (ported from the former C++/Vulkan implementation)
//! parallelizes over JPEG restart intervals; DRI=0 scans (no restart
//! markers) get virtual intervals from a host survey pass that
//! snapshots decoder state every 256 MCUs. The scan is decoded on
//! the GPU once into a full-resolution RGBA buffer kept device-side.
//! `GpuJpeg::resize_*` then runs the PIL-exact separable resampler and
//! the RGB/RGBA pack pass on the GPU, so only the small resized image
//! ever crosses back to the CPU.
//!
//! `IC_NO_GPUJPEG=1` disables it.

mod coeffs;
mod gpu;
mod jenc;
mod jpeg;
mod shaders;

use crate::image_utils::fit_dims;
use image::DynamicImage;
use jpeg::{build_canon, pack_comps, parse_jpg, Jpg};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// GPU jobs (decode/resize/encode) run lock-free: every call owns its
// buffers and waits only on its own submission indices, so an
// arbitrary number of images can be in flight concurrently.

/// Shutdown tracking: `stop()` refuses new GPU work (callers fall back
/// to CPU paths) and `wait_idle()` waits for `GPU_ACTIVE` to reach zero.
/// Every scope that can touch the driver — decode, resize, read-back —
/// holds a [`GpuActive`] guard, so "no driver calls in flight" is
/// checkable, and at process exit the driver's own teardown runs on a
/// quiescent device instead of racing a worker mid-submission.
static GPU_STOPPED: AtomicBool = AtomicBool::new(false);
static GPU_ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Refuse all new GPU work; in-flight calls still finish. Idempotent.
pub fn stop() {
    GPU_STOPPED.store(true, Ordering::SeqCst);
}

/// True once [`stop`] has run — GPU work must not start.
pub fn stopped() -> bool {
    GPU_STOPPED.load(Ordering::SeqCst)
}

/// Wait until no thread is inside a GPU call. Returns `false` on
/// timeout (a wedged driver call) — the caller decides how fatal that is.
pub fn wait_idle(max: std::time::Duration) -> bool {
    let t0 = std::time::Instant::now();
    while GPU_ACTIVE.load(Ordering::SeqCst) > 0 {
        if t0.elapsed() >= max {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    true
}

/// RAII in-flight marker: enter returns None once stopped.
struct GpuActive(());
impl GpuActive {
    fn enter() -> Option<Self> {
        if stopped() {
            return None;
        }
        GPU_ACTIVE.fetch_add(1, Ordering::SeqCst);
        Some(Self(()))
    }
}
impl Drop for GpuActive {
    fn drop(&mut self) {
        GPU_ACTIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

const FILTER_LANCZOS: i32 = coeffs::LANCZOS;

#[inline]
fn disabled() -> bool {
    static D: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *D.get_or_init(|| std::env::var("IC_NO_GPUJPEG").is_ok())
}

/// wgpu device + pipelines are usable (adapter found, push constants
/// supported, WGSL compiled). Lazily initializes on first call.
pub fn available() -> bool {
    !disabled() && !stopped() && gpu::ctx().is_some()
}

/// Returns (width, height, max restart interval across scans) if bytes
/// parse as a JPEG we can handle (baseline or progressive), else None.
/// Pure CPU — never touches the GPU.
pub fn probe(bytes: &[u8]) -> Option<(u32, u32, u32)> {
    let mut j = Jpg::new(bytes);
    if !parse_jpg(&mut j) {
        return None;
    }
    let dri = j
        .scans
        .iter()
        .map(|s| s.dri.max(0) as u32)
        .max()
        .unwrap_or(0);
    let (w, h) = j.out_dims();
    Some((w, h, dri))
}

fn canon(j: &mut Jpg<'_>) {
    for sc in j.scans.iter_mut() {
        for i in 0..4 {
            build_canon(&mut sc.dctab[i]);
            build_canon(&mut sc.actab[i]);
        }
    }
}

fn open_locked(bytes: &[u8]) -> Option<(GpuJpeg, &'static gpu::GpuCtx)> {
    if disabled() {
        return None;
    }
    let mut j = Jpg::new(bytes);
    if !parse_jpg(&mut j) {
        return None;
    }
    let (w, h) = j.out_dims();
    let ctx = gpu::ctx()?;
    canon(&mut j);
    let fg = pack_comps(&j);
    // DRI=0 scans get host "survey" passes that synthesize virtual
    // restart intervals — serial scans decode in parallel anyway.
    let px = ctx.decode_into(&mut j, &fg);
    Some((
        GpuJpeg {
            px: Some(px),
            width: w,
            height: h,
        },
        ctx,
    ))
}

/// Full decode -> RGBA (0xAABBGGRR word order, little-endian
/// R,G,B,0xFF per pixel). Falls back to None on anything unsupported.
pub fn decode_rgba(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let _active = GpuActive::enter()?;
    let (sj, ctx) = open_locked(bytes)?;
    let size = u64::from(sj.width) * u64::from(sj.height) * 4;
    let rgba = ctx.read_buf(sj.px.as_ref()?, size);
    Some((rgba, sj.width, sj.height))
}

/// GPU-resident decoded JPEG. Dropping frees the device buffer — unless
/// [`stop`] ran: a buffer release is a driver call, and during process
/// teardown the driver may already be destructing itself, so the buffer
/// is leaked instead (the device dies with the process regardless).
pub struct GpuJpeg {
    px: Option<wgpu::Buffer>,
    pub width: u32,
    pub height: u32,
}

impl Drop for GpuJpeg {
    fn drop(&mut self) {
        if stopped() {
            std::mem::forget(self.px.take());
        }
    }
}

/// Open a JPEG into a GPU-resident RGBA buffer. Returns None for
/// unsupported JPEGs (non-8-bit, arithmetic, >3 components), when no
/// suitable GPU adapter exists, or after [`stop`].
pub fn open(bytes: &[u8]) -> Option<GpuJpeg> {
    let _active = GpuActive::enter()?;
    open_locked(bytes).map(|(j, _)| j)
}

impl GpuJpeg {
    fn resize_packed(&self, max_w: u32, max_h: u32, n_comp: u32) -> Option<(Vec<u8>, u32, u32)> {
        let _active = GpuActive::enter()?;
        let (ow, oh) = fit_dims(self.width, self.height, max_w, max_h);
        let ctx = gpu::ctx()?;
        let out = ctx.resize_pack(
            self.px.as_ref()?,
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

    /// Resize-to-fit on GPU into an RGBA-word buffer that stays
    /// device-resident — for feeding [`encode_jpeg_buf`] or further
    /// GPU passes without a host roundtrip.
    pub fn resize_to_buffer(&self, max_w: u32, max_h: u32) -> Option<(wgpu::Buffer, u32, u32)> {
        let _active = GpuActive::enter()?;
        let (ow, oh) = fit_dims(self.width, self.height, max_w, max_h);
        let ctx = gpu::ctx()?;
        let buf = ctx.resize_to_buffer(
            self.px.as_ref()?,
            self.width,
            self.height,
            ow,
            oh,
            FILTER_LANCZOS,
        );
        Some((buf, ow, oh))
    }

    /// Baseline-encode a JPEG of `self` at `quality` (1-100, 4:2:0 or
    /// gray) entirely on the GPU. Returns JFIF bytes.
    pub fn encode_jpeg(&self, quality: i32) -> Option<Vec<u8>> {
        let _active = GpuActive::enter()?;
        let ctx = gpu::ctx()?;
        let px = self.px.as_ref()?;
        let (seg_buf, tab) = ctx.encode_into(px, self.width, self.height, false, quality);
        let mx = self.width.div_ceil(16);
        let my = self.height.div_ceil(16);
        let raw = ctx.read_buf(&seg_buf, enc_buf_len(self.width, self.height, false));
        Some(jenc::assemble_jpeg(
            &raw,
            self.width,
            self.height,
            mx,
            my,
            false,
            &tab,
        ))
    }

    /// Resize-to-fit + JPEG-encode, all on GPU: decode, resample and
    /// entropy-encode never bring pixels to the host — only the JPEG
    /// bytes and (via [`resize_to_buffer`]) nothing else cross back.
    /// Returns (jpeg, w, h).
    pub fn encode_resized(
        &self,
        max_w: u32,
        max_h: u32,
        quality: i32,
    ) -> Option<(Vec<u8>, u32, u32)> {
        let _active = GpuActive::enter()?;
        let ctx = gpu::ctx()?;
        let (buf, ow, oh) = self.resize_to_buffer(max_w, max_h)?;
        let (seg_buf, tab) = ctx.encode_into(&buf, ow, oh, false, quality);
        let (mx, my) = (ow.div_ceil(16), oh.div_ceil(16));
        let raw = ctx.read_buf(&seg_buf, enc_buf_len(ow, oh, false));
        Some((
            jenc::assemble_jpeg(&raw, ow, oh, mx, my, false, &tab),
            ow,
            oh,
        ))
    }
}

/// Byte size of the raw encoder output buffer for an `w`x`h` image.
fn enc_buf_len(w: u32, h: u32, gray: bool) -> u64 {
    let (mx, my) = if gray {
        (w.div_ceil(8), h.div_ceil(8))
    } else {
        (w.div_ceil(16), h.div_ceil(16))
    };
    let total = u64::from((mx * my).max(1));
    total * 4 + total * 4 * u64::from(jenc::SEG_WORDS)
}

/// One-shot JPEG encode of CPU-side RGBA bytes (0xAABBGGRR or packed
/// R,G,B,A per pixel). Uploads, encodes on GPU, returns JFIF bytes.
pub fn encode_rgba(rgba: &[u8], w: u32, h: u32, quality: i32) -> Option<Vec<u8>> {
    let _active = GpuActive::enter()?;
    if disabled() || w == 0 || h == 0 || rgba.len() != (w as usize) * (h as usize) * 4 {
        return None;
    }
    let ctx = gpu::ctx()?;
    let src = ctx.upload_rgba(rgba, w, h);
    let (seg_buf, tab) = ctx.encode_into(&src, w, h, false, quality);
    let (mx, my) = (w.div_ceil(16), h.div_ceil(16));
    let raw = ctx.read_buf(&seg_buf, enc_buf_len(w, h, false));
    Some(jenc::assemble_jpeg(&raw, w, h, mx, my, false, &tab))
}
