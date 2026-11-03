// Build script: compile the vendored Vulkan JPEG decoder when the
// system has libvulkan + libshaderc (pkg-config). Otherwise compile
// a stub that always reports "unsupported" so decode_image_bytes
// transparently falls back to the CPU decoder.
fn main() {
    println!("cargo:rerun-if-changed=native/gpujpegdec.cpp");
    println!("cargo:rerun-if-changed=native/vkmini.hpp");
    println!("cargo:rerun-if-changed=native/gpujpeg_stub.c");

    let have = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("vulkan")
        .is_ok()
        && pkg_config::Config::new()
            .cargo_metadata(false)
            .probe("shaderc")
            .is_ok();

    if have {
        cc::Build::new()
            .file("native/gpujpegdec.cpp")
            .cpp(true)
            .flag("-std=c++23")
            .flag("-O2")
            .warnings(false)
            .compile("gpujpegdec");
        println!("cargo:rustc-link-lib=vulkan");
        println!("cargo:rustc-link-lib=shaderc");
        println!("cargo:rustc-cfg=gpujpeg");
    } else {
        cc::Build::new()
            .file("native/gpujpeg_stub.c")
            .compile("gpujpegdec");
        println!("cargo:warning=vulkan/shaderc not found - GPU JPEG decode disabled");
    }
}
