//! GPU JPEG decode via the vendored Vulkan compute decoder
//! (native/gpujpegdec.cpp — ported from VulkanExamples/p80_gpujpegdec,
//! bit-exact vs libjpeg-turbo).
//!
//! Used only for baseline JPEGs WITH restart markers: restart
//! intervals are what make the GPU entropy decode parallel (~430
//! MP/s vs ~33 MP/s for libjpeg-turbo). DRI=0 files have a single
//! serial entropy stream — slower on GPU than zune's CPU decoder,
//! so those stay on the CPU path.
//!
//! [`GpuJpeg`] sessions additionally keep the decoded image
//! GPU-resident: preview/thumbnail resizes run the PIL-exact compute
//! resample straight off the decoded buffer and only the small packed
//! output round-trips through the host (ported from
//! VulkanExamples/p81_resize — bit-exact vs Pillow LANCZOS).
//!
//! Compiled in only when libvulkan + libshaderc are present
//! (build.rs probes via pkg-config); otherwise a stub returns
//! "unsupported" and callers fall back transparently.

use std::ffi::c_int;
use std::sync::Mutex;

#[cfg(gpujpeg)]
mod ffi {
    use super::*;
    extern "C" {
        pub fn gpujpeg_probe(
            data: *const u8,
            len: usize,
            w: *mut c_int,
            h: *mut c_int,
            dri: *mut c_int,
        ) -> c_int;
        pub fn gpujpeg_decode(
            data: *const u8,
            len: usize,
            out: *mut u8,
            require_restart: c_int,
        ) -> c_int;
        pub fn gpujpeg_open(
            data: *const u8,
            len: usize,
            require_restart: c_int,
            w: *mut c_int,
            h: *mut c_int,
        ) -> c_int;
        pub fn gpujpeg_resize(
            sid: c_int,
            out_w: c_int,
            out_h: c_int,
            n_comp: c_int,
            filter: c_int,
            out: *mut u8,
        ) -> c_int;
        pub fn gpujpeg_close(sid: c_int);
    }
}

/// One Vulkan device + one pipeline set shared process-wide; decodes
/// are serialized — the single GPU queue serializes anyway.
static GPU_LOCK: Mutex<()> = Mutex::new(());

/// Whether the native decoder was compiled in (vulkan+shaderc found).
pub fn available() -> bool {
    cfg!(gpujpeg)
}

fn disabled() -> bool {
    std::env::var_os("IC_NO_GPUJPEG").is_some()
}

/// Probe JPEG headers (pure CPU parse — never touches Vulkan).
/// Returns (w, h, dri) for decodable baseline JPEGs.
#[cfg(gpujpeg)]
pub fn probe(bytes: &[u8]) -> Option<(u32, u32, u32)> {
    let (mut w, mut h, mut dri) = (0 as c_int, 0 as c_int, 0 as c_int);
    let rc = unsafe { ffi::gpujpeg_probe(bytes.as_ptr(), bytes.len(), &mut w, &mut h, &mut dri) };
    (rc == 0 && w > 0 && h > 0).then(|| (w as u32, h as u32, dri as u32))
}

/// GPU-decode a JPEG to RGBA. `Some` only for baseline JPEGs with
/// restart markers when the native decoder is compiled in; every
/// other input returns None (caller falls back to the CPU path).
pub fn decode_rgba(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    if disabled() {
        return None;
    }
    #[cfg(gpujpeg)]
    {
        let (w, h, dri) = probe(bytes)?;
        if dri == 0 {
            return None; // serial entropy stream — CPU decoder is faster
        }
        let mut out = vec![0u8; w as usize * h as usize * 4];
        let _guard = GPU_LOCK.lock().ok()?;
        let rc = unsafe { ffi::gpujpeg_decode(bytes.as_ptr(), bytes.len(), out.as_mut_ptr(), 1) };
        (rc == 0).then(|| (out, w, h))
    }
    #[cfg(not(gpujpeg))]
    {
        let _ = bytes;
        None
    }
}

/// gpujpeg_resize filter id — lanczos matches the quality tier
/// fast_image_resize's default Lanczos3 provides on the CPU path.
const FILTER_LANCZOS: c_int = 5;

/// A GPU-resident decoded JPEG: the full-resolution RGBA stays in a
/// GPU buffer and each `resize_*` produces only the small packed
/// output — the full-res pixels never round-trip through the host.
pub struct GpuJpeg {
    sid: c_int,
    pub width: u32,
    pub height: u32,
}

/// Open a decode session: decode on the GPU and keep the result
/// GPU-resident for follow-on resizes. `Some` under the same
/// conditions as [`decode_rgba`] (baseline + restart markers).
pub fn open(bytes: &[u8]) -> Option<GpuJpeg> {
    if disabled() {
        return None;
    }
    #[cfg(gpujpeg)]
    {
        let _guard = GPU_LOCK.lock().ok()?;
        let (mut w, mut h) = (0 as c_int, 0 as c_int);
        let sid = unsafe {
            ffi::gpujpeg_open(bytes.as_ptr(), bytes.len(), 1, &mut w, &mut h)
        };
        (sid > 0 && w > 0 && h > 0)
            .then(|| GpuJpeg { sid, width: w as u32, height: h as u32 })
    }
    #[cfg(not(gpujpeg))]
    {
        let _ = bytes;
        None
    }
}

impl GpuJpeg {
    /// GPU-resize to fit `max_w`x`max_h` (aspect preserved — same
    /// target dims as `image_utils::resize_to_fit`), returning packed
    /// RGB8 ready for JPEG encoding.
    pub fn resize_rgb(&self, max_w: u32, max_h: u32) -> Option<(Vec<u8>, u32, u32)> {
        self.resize_packed(max_w, max_h, 3)
    }

    /// [`resize_rgb`] as a `DynamicImage::ImageRgb8`.
    pub fn resize_image(&self, max_w: u32, max_h: u32) -> Option<image::DynamicImage> {
        let (rgb, w, h) = self.resize_rgb(max_w, max_h)?;
        image::RgbImage::from_raw(w, h, rgb).map(image::DynamicImage::ImageRgb8)
    }

    /// GPU-resize like [`resize_rgb`] but returning RGBA8 — for callers
    /// feeding RGBA consumers (e.g. texture upload) directly.
    pub fn resize_rgba(&self, max_w: u32, max_h: u32) -> Option<(Vec<u8>, u32, u32)> {
        self.resize_packed(max_w, max_h, 4)
    }

    /// [`resize_rgba`] as a `DynamicImage::ImageRgba8`.
    pub fn resize_image_rgba(&self, max_w: u32, max_h: u32) -> Option<image::DynamicImage> {
        let (rgba, w, h) = self.resize_rgba(max_w, max_h)?;
        image::RgbaImage::from_raw(w, h, rgba).map(image::DynamicImage::ImageRgba8)
    }

    fn resize_packed(&self, max_w: u32, max_h: u32, n_comp: c_int) -> Option<(Vec<u8>, u32, u32)> {
        #[cfg(gpujpeg)]
        {
            let (out_w, out_h) =
                crate::image_utils::fit_dims(self.width, self.height, max_w, max_h);
            let mut out = vec![0u8; out_w as usize * out_h as usize * n_comp as usize];
            let _guard = GPU_LOCK.lock().ok()?;
            let rc = unsafe {
                ffi::gpujpeg_resize(
                    self.sid,
                    out_w as c_int,
                    out_h as c_int,
                    n_comp,
                    FILTER_LANCZOS,
                    out.as_mut_ptr(),
                )
            };
            return (rc == 0).then(|| (out, out_w, out_h));
        }
        #[cfg(not(gpujpeg))]
        {
            let _ = (max_w, max_h, n_comp);
            None
        }
    }
}

impl Drop for GpuJpeg {
    fn drop(&mut self) {
        #[cfg(gpujpeg)]
        {
            let _guard = GPU_LOCK.lock().ok();
            unsafe { ffi::gpujpeg_close(self.sid) }
        }
    }
}
