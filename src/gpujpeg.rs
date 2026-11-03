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
//! Compiled in only when libvulkan + libshaderc are present
//! (build.rs probes via pkg-config); otherwise a stub returns
//! "unsupported" and decode_image_bytes falls back transparently.

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
