# ImageCropper on Synology NAS

How to build a portable binary and run it on a Synology (DSM) NAS.

## Why a static build

DSM ships an old glibc (2.36 on DSM 7.x) while binaries built on a modern
distro require `GLIBC_2.4x` symbols, and `/tmp` is mounted `noexec`.
A fully static binary sidesteps both problems — no interpreter, no shared
library requirements at all.

## Build

```sh
cargo rustc --release --no-default-features --bin imagecropper -- \
    -C target-feature=+crt-static
```

- `--no-default-features` drops the `avif-decode` cargo feature (dav1d,
  the only C dependency). AVIF *encoding* stays enabled — it is pure Rust
  (`ravif`/`rav1e` via `image/avif`).
- `-C target-feature=+crt-static` statically links glibc, producing a
  `static-pie` binary with zero `NEEDED` entries. Verify with:
  `file target/release/imagecropper` → `static-pie linked`.

On machines with limited RAM, run the build under the memory cap described
in `AGENTS.md`:

```sh
systemd-run --user --scope --quiet -p MemoryMax=6G --unit ic-build --collect \
    bash -c 'CARGO_BUILD_JOBS=2 cargo rustc --release --no-default-features --bin imagecropper -- -C target-feature=+crt-static'
```

## What this build can and cannot do

- **AVIF decode: off.** `.avif` inputs fail to decode (they are still
  listed in scans; the error is per-file). AVIF **encode still works** —
  `-f avif` output is unaffected.
- **GPU: off.** Static glibc cannot `dlopen`, so wgpu/Vulkan (GPU JPEG
  decode, native UI rendering) is unavailable. All image operations fall
  back to CPU automatically; `gpucheck` reports this cleanly.
- Everything else works normally: JPEG/PNG/WebP/TIFF/GIF/BMP/ICO decode
  and encode, EXIF orientation, the `--serve` web UI, preview caches.

## Deploy

The NAS sshd has no SFTP subsystem — `scp` must use the legacy protocol
(`-O`):

```sh
scp -O target/release/imagecropper uli@NAS:ic-static
```

`/tmp` is `noexec` on DSM, so run the binary from your home directory (or
anywhere on a mounted volume). `~/ic-static` above lands in
`/var/services/homes/uli/` and is executable immediately.

Sanity check on the NAS:

```sh
~/ic-static --version    # imagecropper 0.4.1
~/ic-static gpucheck     # reports "wgpu adapter: NOT FOUND" — expected
```

## Run headless

A NAS has no display, so use `--serve` (web UI in a browser) or `-G`
(non-interactive pipeline). `-G` is required with `--serve` to suppress
the browser auto-open.

```sh
# Web UI on the LAN — prints a URL carrying a random auth token
~/ic-static album /volume1/photos --serve=0.0.0.0:8790 -G

# Fixed password instead of a random token
~/ic-static album /volume1/photos --serve=0.0.0.0:8790 -G -p secret

# No auth at all (trusted LAN only!)
~/ic-static album /volume1/photos --serve=0.0.0.0:8790 -G --auth none

# Batch-resave everything to JPEG
~/ic-static crop /volume1/photos -G --resave -f jpg

# Pregenerate album previews only
~/ic-static album /volume1/photos -G
```

- `--no-gpu` is unnecessary in this build (the GPU paths are already
  dead) but harmless.
- For always-on use, place the binary on a volume share and add a DSM
  Task Scheduler entry (e.g. `~/ic-static album /volume1/photos
  --serve=0.0.0.0:8790 -G -p secret` on boot).

## Rebuilding with AVIF decode

If `.avif` input decoding is needed, the static link requires a
`libdav1d.a`. `SYSTEM_DEPS_DAV1D_BUILD_INTERNAL=always` makes `dav1d-sys`
build dav1d 1.5.0 from source via meson — but getting the resulting
archive onto the `-static` link path needs extra work (the default build
links the system `libdav1d.so`). The alternative is a dynamic build inside
a Debian bookworm container (glibc 2.36 matches DSM exactly); bookworm's
system dav1d is too old, so the internal build is needed there as well,
and the resulting `.so` must be shipped next to the binary.
