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
- Playwright E2E: `cd e2e && npx playwright test` (chromium already installed
  under `~/.cache/ms-playwright`; the config launches `imagecropper album
  --serve=127.0.0.1:8790 -G -m e2e/.move-target e2e/.fixtures` itself and
  regenerates fixtures from `e2e/fixtures.js` inside the webServer command —
  Playwright starts `webServer` *before* `globalSetup`).
