# ImageCropper dev notes

## Build/test under a 6 GB memory cap

Linking is the memory bottleneck on this repo: `rust-lld` uses ~2 GB per
binary and the host has 16 cores, so default parallelism can spawn many
concurrent links (>6 GB). Rust test binaries barely register; it's the links.

Run builds/tests inside a systemd cgroup scope and bound link jobs:

```sh
systemd-run --user --scope --quiet -p MemoryMax=6G --unit ic-build --collect \
    bash -c 'CARGO_BUILD_JOBS=2 cargo test'
```

- `CARGO_BUILD_JOBS=2` keeps ≤2 `lld` invocations in flight (~4 GB worst case).
- If the scope gets `oom-kill`ed during `Compiling`, lower jobs further.
- `dav1d` builds against the system `libdav1d-dev` (installed); the
  `SYSTEM_DEPS_DAV1D_BUILD_INTERNAL=always` fallback is no longer needed.
- GPU JPEG decode: `native/gpujpegdec.cpp` (vendored from
  `VulkanExamples/p80_gpujpegdec`, bit-exact vs libjpeg-turbo) compiles
  in via build.rs when `pkg-config` finds `vulkan` + `shaderc`; a stub
  keeps the build green without them. Only used for baseline JPEGs
  with restart markers (DRI>0) — that's where the parallel GPU path
  beats zune; DRI=0/unsupported fall back automatically.
  `IC_NO_GPUJPEG=1` disables at runtime. Decodes serialize on one
  Vulkan device via a process-wide mutex.
- Playwright E2E: `cd e2e && npx playwright test` (chromium already installed
  under `~/.cache/ms-playwright`; the config launches `imagecropper album
  --serve=127.0.0.1:8790 -G -m e2e/.move-target e2e/.fixtures` itself and
  regenerates fixtures from `e2e/fixtures.js` inside the webServer command —
  Playwright starts `webServer` *before* `globalSetup`).
