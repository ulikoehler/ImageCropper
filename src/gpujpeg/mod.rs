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
//! `IC_NO_GPUJPEG=1` disables it. Software rasterizer adapters
//! (llvmpipe/lavapipe — no real GPU) are auto-detected and disabled:
//! compute there IS the CPU, slower than the zune path. Set
//! `IC_GPUJPEG_SW=1` to force-enable anyway.

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
    // Every storage buffer bound during decode must fit the device's
    // binding-size limit (as small as 128 MB on some adapters). The
    // big ones: coefficient array (128 B/block), RGBA output (4 B/px)
    // and the compressed scan data. Oversized images → CPU fallback.
    let need = (fg.total_blocks as u64 * 128)
        .max(u64::from(w) * u64::from(h) * 4)
        .max(j.data.len() as u64);
    if need > ctx.max_bind {
        return None;
    }
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
    /// device-resident — for feeding further GPU passes without a host
    /// roundtrip. The resample may still be in flight; queue ordering
    /// makes dependent submissions observe it complete.
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

    /// Baseline-encode a JPEG of `self` at `quality` (1-100, 4:2:0)
    /// entirely on the GPU. Returns JFIF bytes.
    pub fn encode_jpeg(&self, quality: i32) -> Option<Vec<u8>> {
        self.encode_jpeg_start(quality)?
            .finish()
            .map(|(j, _, _)| j)
    }

    /// Full-res encode chain — same lazy model as
    /// [`encode_resized_start`].
    pub fn encode_jpeg_start(&self, quality: i32) -> Option<EncodeJob> {
        let _active = GpuActive::enter()?;
        let ctx = gpu::ctx()?;
        if !enc_fits(ctx, self.width, self.height) {
            return None;
        }
        let p = ctx.encode_chain(
            self.px.as_ref()?,
            self.width,
            self.height,
            self.width,
            self.height,
            FILTER_LANCZOS,
            quality,
        );
        Some(EncodeJob {
            p,
            w: self.width,
            h: self.height,
        })
    }

    /// Resize-to-fit + JPEG-encode submitted as ONE GPU chain (resize,
    /// encode, readback-copy ride a single submission). No wait happens
    /// here — [`EncodeJob::finish`] resolves the readback. Submit
    /// several jobs back-to-back and finish them later for genuine
    /// multi-image pipelining: a later finish also implies earlier
    /// jobs are done (in-order queue).
    pub fn encode_resized_start(
        &self,
        max_w: u32,
        max_h: u32,
        quality: i32,
    ) -> Option<EncodeJob> {
        let _active = GpuActive::enter()?;
        let (ow, oh) = fit_dims(self.width, self.height, max_w, max_h);
        let ctx = gpu::ctx()?;
        if !enc_fits(ctx, ow, oh) {
            return None;
        }
        let p = ctx.encode_chain(
            self.px.as_ref()?,
            self.width,
            self.height,
            ow,
            oh,
            FILTER_LANCZOS,
            quality,
        );
        Some(EncodeJob { p, w: ow, h: oh })
    }

    /// Resize-to-fit + JPEG-encode, all on GPU: decode, resample and
    /// entropy-encode never bring pixels to the host — only the JPEG
    /// bytes cross back. Returns (jpeg, w, h).
    pub fn encode_resized(
        &self,
        max_w: u32,
        max_h: u32,
        quality: i32,
    ) -> Option<(Vec<u8>, u32, u32)> {
        self.encode_resized_start(max_w, max_h, quality)?.finish()
    }
}

/// A submitted encode chain — resolves to (jpeg, w, h) on `finish`.
/// Cheap to hold: the GPU keeps working meanwhile, and other jobs can
/// be submitted behind it before this one is finished.
pub struct EncodeJob {
    p: gpu::EncodePending,
    w: u32,
    h: u32,
}

impl EncodeJob {
    /// Wait for this job's chain (covering all earlier queue work) and
    /// assemble the JFIF. Returns (jpeg_bytes, w, h).
    pub fn finish(self) -> Option<(Vec<u8>, u32, u32)> {
        let _active = GpuActive::enter()?;
        let ctx = gpu::ctx()?;
        let raw = ctx.map_wait(self.p.staging.as_ref()?, self.p.len, self.p.idx.clone());
        if raw.is_empty() {
            return None;
        }
        Some((
            jenc::assemble_jpeg(&raw, self.w, self.h, self.p.mx, self.p.my, false, &self.p.tab),
            self.w,
            self.h,
        ))
    }
}

/// Both storage buffers the encode binds — source RGBA (4 B/px) and
/// the segment output (sizes + 1 KiB per MCU) — must fit the device
/// binding limit. Returns false → caller falls back to CPU encoding.
fn enc_fits(ctx: &gpu::GpuCtx, w: u32, h: u32) -> bool {
    let total = u64::from((w.div_ceil(16) * h.div_ceil(16)).max(1));
    (u64::from(w) * u64::from(h) * 4).max(total * 4 + total * 4 * u64::from(jenc::SEG_WORDS))
        <= ctx.max_bind
}

/// One-shot JPEG encode of CPU-side RGBA bytes (packed R,G,B,A per
/// pixel). Uploads, encodes on GPU, returns JFIF bytes.
pub fn encode_rgba(rgba: &[u8], w: u32, h: u32, quality: i32) -> Option<Vec<u8>> {
    let _active = GpuActive::enter()?;
    if disabled() || w == 0 || h == 0 || rgba.len() != (w as usize) * (h as usize) * 4 {
        return None;
    }
    let ctx = gpu::ctx()?;
    if !enc_fits(ctx, w, h) {
        return None;
    }
    let src = ctx.upload_rgba(rgba, w, h);
    let p = ctx.encode_chain(&src, w, h, w, h, FILTER_LANCZOS, quality);
    let raw = ctx.map_wait(p.staging.as_ref()?, p.len, p.idx.clone());
    if raw.is_empty() {
        return None;
    }
    Some(jenc::assemble_jpeg(&raw, w, h, p.mx, p.my, false, &p.tab))
}
