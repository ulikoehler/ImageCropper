# ImageCropper Web Interface — Implementation Plan

Status: DRAFT v2 — transport redesigned around WebSocket push (replaces the
HTTP-fetch model) because bulk image transfer must not depend on HTTP/2 or
browser connection limits.

Scope: a web UI replicating the native album grid and crop UI with
near-native latency, launched via `--serve=ADDR:PORT` on the existing `crop`
and `album` subcommands (no separate `serve` subcommand).

---

## 0. Executive summary

Two processes:

* **Headless server mode** — `imagecropper album --serve=0.0.0.0:12345 <PATHS>`
  (and `crop --serve=…`) runs a Rust server owning the filesystem: scanning,
  filters/sorting, preview generation (`.imagecropper-previews`), full-image
  decode, all destructive actions (delete/crop/save) — reusing `Thumbnailer`,
  `Deleter`, `Saver`, `Loader`, `fs_utils`. `-G`/`--no-gui` runs the
  non-interactive pipeline with no window at all (§5.0).
* **`web` frontend** — **Rust → WASM** using **`wgpu` on WebGPU** for
  rendering — *not* egui-to-wasm. The UI layer (grid, selection, HUD text)
  is a purpose-built immediate renderer.

**Transport: two WebSockets, no bulk HTTP at all.**

```
ws://…/data   — control + events + thumbnail push   (small binary frames)
ws://…/bulk   — manifest + full-resolution images   (chunked binary frames)
http://…/     — ONLY static assets (index.html / app.js / app.wasm) + WS upgrade
```

Why this beats HTTP fetching (the user's constraint is correct):

* **Browser connection limits don't apply.** 10k thumbs over HTTP means
  queueing through ~6 conn/host on HTTP/1.1 and is still request-heavy on
  HTTP/2. One WS carries everything.
* **Server push = free "preload ALL".** The server streams previews as it
  generates them — no request per image, no round-trip per tile. The client
  just reorders priorities via cheap hint messages.
* **No per-request overhead.** A `THUMB` frame is a ~20-byte header + raw
  JPEG — no headers/cookies/content negotiation per image.
* **Binary frames.** No base64, no multipart parsing — payload bytes go
  straight into `createImageBitmap(new Blob([frame.payload]))`.
* **Backpressure is explicit** (credit-based, §6.4) instead of hoping TCP +
  fetch scheduling cooperate.
* **Works over plain HTTP/1.1 proxies** — no HTTP/2 assumption anywhere.

The critical latency path stays:

```
server disk cache → server RAM → WS frame → createImageBitmap (browser
decoder, off-main-thread) → copyExternalImageToTexture (GPU upload) →
instanced quad draw
```

Estimated implementation: **~11–14k LOC** + ~2–3k tests. File-by-file in §14.

---

## 1. Goals, non-goals

| # | Goal | Target |
|---|------|--------|
| G1 | Album grid identical to native album mode | same layout, keys, overlays |
| G2 | Crop mode identical to native crop | same handles/keys/multicrop |
| G3 | Interaction latency | input→visible ≤ 16 ms local, ≤ 40 ms LAN |
| G4 | 60 fps scroll, zero gray tiles steady-state | prefetch hit-rate ≥ 98 % |
| G5 | Preview persistence shared with native | reuse `.imagecropper-previews`, same signature scheme |
| G6 | All scan/sort/filter flags | shared `ScanArgs` |
| G7 | Queued, acknowledged destructive ops | `.imagecropper-trash` semantics |
| G8 | All bulk data over WebSocket | HTTP only for asset bootstrap |

Non-goals (v1): multi-user/auth, non-crop editing, video, offline PWA.

---

## 2. Latency budget

| Interaction | Path | Budget |
|---|---|---|
| Arrow/click focus+select | event → state → rAF draw | ≤ 16 ms (pure local) |
| Delete key | optimistic fade; WS ack later | ≤ 16 ms visible |
| Tile visible (in GPU atlas) | draw only | ≤ 16 ms |
| Tile visible (bitmap in T1) | `copyExternalImageToTexture` 0.5–2 ms | ≤ 16 ms |
| Tile visible (pushed JPEG in T2) | `createImageBitmap` 3–15 ms | ≤ 40 ms |
| Tile visible (server generating) | push when ready | ≤ 150 ms |
| Open crop (full-res prefetched) | decode+upload | ≤ 60 ms |
| Open crop (cold) | bulk fetch+decode | ≤ 400 ms, preview shown instantly |
| Save crop | optimistic advance | ≤ 40 ms |

Invariant: **no user input ever waits on the network.** Inputs mutate local
state and draw next rAF; WS traffic only fills caches and confirms
optimistic changes.

---

## 3. Architecture overview

```
┌──────────────────────────────────────────────────────────────────┐
│  imagecropper album --serve=0.0.0.0:12345 [OPTIONS] <PATHS>      │
│  ┌──────────────┐  ┌───────────────┐  ┌───────────────────────┐  │
│  │ ScanService  │  │ PreviewStore  │  │ ActionQueue           │  │
│  │ (ScanArgs,   │  │ .imagecropper │  │ delete/crop/save via  │  │
│  │ filters,     │  │ -previews     │  │ Deleter/Saver/Loader  │  │
│  │  sorting)    │  │ + RAM LRU     │  │                       │  │
│  └──────┬───────┘  └──────┬────────┘  └──────────┬────────────┘  │
│         │                 │                      │               │
│  ┌──────┴─────────────────┴──────────────────────┴────────────┐  │
│  │  axum: static assets + 2 WS endpoints                      │  │
│  │   /data  control, events, THUMB push (priority-scheduled)  │  │
│  │   /bulk  MANIFEST + FULL image transfer (chunked)          │  │
│  │   PushScheduler: interleaves client wants + proactive fill │  │
│  └────────────────────────────┬──────────────────────────────┘  │
└───────────────────────────────┼──────────────────────────────────┘
                                │ 2×WS (+1 asset fetch at boot)
┌───────────────────────────────┼──────────────────────────────────┐
│  Browser — Rust/WASM client   │                                  │
│  ┌──────────────┐  ┌──────────┴────────┐  ┌───────────────────┐  │
│  │ net::data    │  │ net::bulk         │  │ render::Pipeline  │  │
│  │ events,      │  │ manifest,         │  │ wgpu: atlas mgr,  │  │
│  │ thumbs mux,  │  │ full-img chunks   │  │ instanced quads,  │  │
│  │ credits      │  │                   │  │ glyphon text      │  │
│  └──────┬───────┘  └─────────┬─────────┘  └────────┬──────────┘  │
│  ┌──────┴───────────────────┴─────────────────────┴───────────┐  │
│  │ cache::Store (bitmap LRU, inflight) + preload::Scheduler   │  │
│  └───────────────────────────────────────────────────────────┘  │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │ state::Album (layout, selection, focus) / state::Crop      │  │
│  └───────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────────┘
```

Static assets are embedded in the binary (`include_bytes!` of the `trunk
build --release` output), so deployment remains `cargo install` + run.

### Why two sockets instead of one

A single WS is strictly ordered — a 20 MB full image would head-of-line
block thumb frames behind it. Splitting bulk transfers (manifest chunks,
full images) onto `/bulk` keeps `/data` latency ≤ a few ms of queued small
frames. Two connections is still way below any browser limit and requires
no HTTP/2.

---

## 4. Why WebGPU — assessment

The user's proposal is correct, with one refinement:

* **Rendering: yes.** `wgpu` compiles to WebGPU on `wasm32-unknown-unknown`;
  one render codebase serves native + web. Instanced textured quads → 60 fps
  at 10k+ tiles; GPU-resident atlases make scroll/zoom pure vertex math.
* **The real latency lever is the upload/decode path**, which is not wgpu:
  `createImageBitmap(blob)` decodes on browser-internal (often hardware)
  threads; `queue.copyExternalImageToTexture` uploads straight to a
  `wgpu::Texture` with no CPU RGBA copy. Fallback: WASM decode workers +
  `queue.write_texture`.
* **egui→wasm rejected** (agreed): CPU repaints retained-mode style each
  frame; fine for forms, wrong for a kinetic 10k-tile grid. Our UI needs
  ~15 primitives — cheaper to own than egui's pipeline + event shims.

Rejected: DOM `<img>` grid (per-tile layout/GC cost, no GPU-resident cache),
Canvas2D primary (CPU), WebGL2 primary (no `copyExternalImageToTexture` —
kept as fallback), server-rendered video stream (latency, codec cost).

---

## 5. Server: `--serve` / `--no-gui` on `crop` and `album`

**No `serve` subcommand.** Serving is a flag on the existing subcommands —
the web UI inherits the subcommand's file set, filters, sorting, and
mode semantics:

```
imagecropper album --serve=0.0.0.0:12345 -r /photos    # album web UI
imagecropper crop  --serve=127.0.0.1:8737 IMG*.jpg     # web crop session
imagecropper album -G -r /photos                       # headless preview generation
imagecropper crop  -G --resave -q 85 /photos           # headless batch resave
```

Both subcommands flatten in a shared `HeadlessArgs`:

```rust
#[derive(Args, Clone, Default)]
struct HeadlessArgs {
    /// No GUI. Alone: run the non-interactive pipeline and exit.
    /// With --serve: start the server without auto-opening a browser.
    #[arg(short='G', long)]
    no_gui: bool,
    /// Serve the web UI at ADDR:PORT instead of opening the native GUI.
    /// Binds loopback by default via 127.0.0.1:PORT form; 0.0.0.0 for LAN.
    #[arg(long, value_name="ADDR:PORT")]
    serve: Option<SocketAddr>,
    /// Bearer token required on WS HELLO (random + printed if --serve and unset)
    #[arg(long)]
    token: Option<String>,
    /// Initial per-client push credit (KB) — see §6.4
    #[arg(long, default_value_t=4096)]
    push_credit_kb: u32,
}
```

### 5.0 Invocation mode matrix

| Flags | `album` | `crop` |
|---|---|---|
| (none) | native egui grid (today) | native egui crop (today) |
| `--serve=A:P` | web server; client boots into **album grid**; auto-opens browser | web server; client boots directly into **crop mode** on the file set (Esc still drops to the grid — the web client has both modes); auto-opens browser |
| `-G` | scan + pregenerate **all** previews into `.imagecropper-previews`, then exit (cache warming, CI) | non-interactive batch: apply `--resave`/format/quality to all matched files, then exit (equivalent to today's `Resave` path without a window) |
| `-G --serve=A:P` | server, no browser auto-open | server, no browser auto-open |

* `--serve` implies no native window in all cases.
* Auto-open uses the `webbrowser` crate (`open::that`); suppressed by `-G`.
* The manifest carries `mode: "album" | "crop"` so the client picks its
  initial mode (§6.3).
* `-G` without `--serve` and without any work (e.g., `album -G` on an
  already-cached dir) exits immediately after the cache check.

### 5.1 Shared-code moves

* `ScanArgs`, `collect_sorted_files`, `sort_files`, `exif_cmp` move from
  `main.rs` → new `src/scan.rs` (also makes sorting testable).
* Preview naming/signature helpers (`preview_file_name`, `preview_file_prefix`,
  `previews_dir_for`, `remove_previews_for`, `source_signature`) move from
  `album::thumbnails` → `src/fs_utils.rs` (or `fs_utils::previews`) so album +
  server share them. Add `preview_sig_u64(path) -> u64` for the manifest.
* `album::thumbnails::produce_thumbnail` generalizes into `PreviewStore`:
  same worker pool + returns **encoded JPEG bytes** (for WS) and/or
  `DynamicImage`/`ColorImage` (for native).

### 5.2 Runtime state

```rust
struct ServerState {
    manifest: Arc<RwLock<Manifest>>,         // versioned
    previews: PreviewStore,                  // workers + disk cache + RAM LRU<Vec<u8>>
    full: FullImageStore,                    // decode/re-encode, small LRU
    actions: ActionQueue,                    // sig-checked delete/crop/save
    push: PushScheduler,                     // per-client priority queue + credits
    events: broadcast::Sender<ServerEvent>,
    opts: ServeConfig,                     // built from the subcommand's HeadlessArgs
    mode: ServeMode,                       // Album | Crop — from the subcommand used
}
```

`PreviewStore`: existing `.imagecropper-previews` disk cache + worker pool +
`lru::LruCache<PathBuf, Arc<Vec<u8>>>` (encoded JPEG bytes, ~2048 items) in
front of disk — hot thumbs never hit disk twice.

`FullImageStore`: `decode_image_bytes` + `resize_to_fit(3840²)` (same policy
as `Loader`), LRU ~8 items; optional `?as=jpg&q=…`-style re-encode selected
per-request to cut transfer on huge sources.

### 5.3 HTTP surface (deliberately tiny)

```
GET /            → index.html (embedded)
GET /app.js      → embedded, immutable
GET /app_bg.wasm → embedded, immutable
GET /healthz     → 200
WS  /data        → interactive channel (§6)
WS  /bulk        → manifest + full-image channel (§6)
```

No `/api/thumb`, no `/api/full`, no `/api/action`, no thumb-batch endpoint —
all of it moved onto WS. The only other HTTP ever used is the initial
page+asset load (3 requests, HTTP/1.1-safe).

### 5.4 Proactive preview generation + fs watching

* On manifest build, the server queues **every** path in `PreviewStore` in
  manifest order — previews are generated ahead of any client asking, then
  *pushed* (§6.4) to connected clients by the PushScheduler.
* `notify` watcher on scanned roots (respecting `-r`): new/changed file →
  signature recompute → regen → `manifest-patch` + `thumb` push; deleted →
  `file-gone`. Debounced 250 ms; manifest version bumps.
* PreviewStore emits "thumb-ready" signals that let the scheduler push a
  freshly generated preview without waiting for a client request — this is
  the "preload ALL" engine.

### 5.5 ActionQueue

```rust
enum Action {
    Delete   { id: u32, sig: u64 },
    CropSave { id: u32, sig: u64, selections: Vec<[u32;4]>, rotate: RotateOp, format, quality },
    Resave   { id: u32, sig: u64, format, quality },
}
```

* Precondition: `sig` must equal `preview_sig_u64(path)` else →
  `action-done{ok:false, code:"stale"}` and the client rolls back.
* `CropSave` = `build_output_image` + `Saver::queue_save` (identical code
  path to native). Crop rects are **image-space u32** in natural-size
  coordinates — pixel-exact, no float round-trip.
* Completions broadcast `action-done{tag, ok, new_id?, new_sig?, bytes?}`.
  Delete additionally emits `file-gone` to *all* clients.

### 5.6 PushScheduler (server → client streaming core)

Per connected `/data` client:

```rust
struct PushQueue {
    wants: VecDeque<u32>,          // client-requested ids (highest priority)
    boosts: BTreeMap<u32, u64>,    // id → priority score from client hints
    cursor: usize,                 // manifest-order fill position
    credit_bytes: i64,             // client-granted budget
}
```

* Ready thumbs are sent in this order: explicit `WANT` ids → hinted
  (viewport/focus) ids by score → manifest `cursor` order — all throttled by
  the client's `credit_bytes` and a global pacing rate (default ~64 MB/s,
  configurable).
* `VIEWPORT`/`FOCUS` hints just update `boosts` — sub-millisecond, no data.
* `PAUSE`/`RESUME` + `CREDIT{bytes}` implement flow control (§6.4).
* A `Progress` event (`{queued,done}` counts) rides the control stream so
  the HUD can show "previews 421/1234" like the native status line.

---

## 6. Protocol — binary WebSocket frames

Design: **one frame type byte** then either a JSON payload (control) or a
binary header + opaque payload (data). Little-endian u32s.

```
[ u8 type ][ u32 req_id ][ u32 flags ][ u32 len ][ payload… ]
```

`req_id` multiplexes request/response on one socket (0 = unsolicited/push).
`flags` carries `LAST=0x1`, `SEQ` handled by an explicit u32 for chunked
messages (see FULL/MANIFEST).

### 6.1 `/data` frames — control + thumbs

Server → client:

| Type | Name | Payload |
|---|---|---|
| 0x01 | `EVENT` | UTF-8 JSON: `{"ev":…}` — progress, action-done, file-gone, manifest-patch-ref, hello-ack |
| 0x02 | `THUMB` | `{u32 id, u32 sig_hi, u32 sig_lo, u32 jpeg_len, bytes}` — one preview, pushed or wanted |
| 0x03 | `THUMB_BATCH` | `{u16 count} × {u32 id, u32 jpeg_len, bytes}` — coalesced when draining a want-list (≤64) |

Client → server (all JSON except `WANT` ids may be binary-packed):

| Type | Name | Payload |
|---|---|---|
| 0x10 | `HELLO` | `{"token":…,"manifest_version":N}` — resume: server skips unchanged manifest |
| 0x11 | `WANT` | `{u16 n} × u32 id` (+1 priority byte) — explicit needs (visible tiles) |
| 0x12 | `VIEWPORT` | `{u32 first, u32 last, i8 dir}` — hint; reorder boosts only |
| 0x13 | `FOCUS` | `{u32 id}` — hint; full-res prefetch trigger |
| 0x14 | `ACTION` | JSON `{tag, delete/crop/resave {id,sig,…}}` |
| 0x15 | `CREDIT` | `{u64 bytes}` — grant push budget (§6.4) |
| 0x16 | `PAUSE`/`RESUME` | — | suspend/resume push (hidden tab, fling) |
| 0x17 | `PING` | `{u64 t}` |
| 0x18 | `CANCEL` | `{u32 req_id}` — abort an in-flight bulk send |

Server → client on `/data` also: `PONG` (0x06) echoing `t`.

### 6.2 `/bulk` frames — manifest + full images

Server → client:

| Type | Name | Payload |
|---|---|---|
| 0x20 | `MANIFEST` | `seq,last` chunked bincode `Manifest` (§6.3) |
| 0x21 | `FULL_CHUNK` | `{u32 req_id, u32 seq, u8 last} + bytes` — ≤256 KB chunks |
| 0x22 | `FULL_HEAD` | `{u32 req_id, u32 id, u64 sig, u32 w, u32 h, u8 fmt, u64 total_len}` — precedes chunks |
| 0x23 | `MANIFEST_PATCH` | bincode `{version, upsert:[…], remove:[u32]}` |

Client → server:

| Type | Name | Payload |
|---|---|---|
| 0x28 | `GET_MANIFEST` | `{u32 req_id, u32 known_version}` |
| 0x29 | `GET_FULL` | `{u32 req_id, u32 id, u64 sig, u8 want_fmt, u8 q}` |
| 0x2A | `CANCEL` | `{u32 req_id}` |

Chunk size 256 KB keeps a full image from occupying the socket for more than
a few ms on LAN; sequential `req_id`s allow up to 4 concurrent full fetches
(interleaved chunks) without starving `/data` (different socket anyway).

### 6.3 Manifest (bincode schema)

```rust
struct Manifest {
    version: u32,
    mode: ServeMode,            // Album | Crop — initial client mode
    generated_ms: u64,
    roots: Vec<String>,
    items: Vec<ManifestItem>,
}
struct ManifestItem {          // ~48 B encoded ⇒ ~480 KB at 10k items
    id: u32,
    name: CompactString,       // file name only
    dir_index: u16,            // interned parent dirs
    w: u32, h: u32,            // natural size (0,0 until sniffed/patched)
    bytes: u64,
    sig: u64,                  // same signature as preview filenames
}
```

* `dir_index` → `dirs: Vec<String>` interned table keeps items small.
* `w/h` via header-sniff at scan time (`image::io::Reader` dimensions-only
  read — ~µs/file on a thread pool). Failing that, 0×0 + aspect=1 hint +
  `MANIFEST_PATCH` when known — boot never blocks on it.
* Compressed? 10k items ≈ 500 KB raw — acceptable unchunked-ish; still sent
  as `MANIFEST` chunks so a 500k-dir won't produce a 25 MB single frame.
  `permessage-deflate` negotiated but only beneficial for JSON control; keep
  it **off** for data frames (JPEGs don't compress) — negotiate a per-frame
  flag… simpler: do not negotiate deflate at all; JPEG payloads dominate.

### 6.4 Flow control

* Client grants `CREDIT{bytes}` — e.g., refill to 8 MB whenever its
  `bufferedAmount` < 4 MB. Server decrements as it sends; at 0 it holds push
  (control/events unaffected).
* `PAUSE` on `document.hidden` or scroll-fling > threshold; `RESUME` after.
* `CANCEL{req_id}` drops in-flight bulk sends (navigated away from crop).
* Events are coalesced server-side: `progress` ≤ 10/s; `thumb` frames never
  coalesce (each carries distinct bytes).

### 6.5 Reconnect & resume

WS drop → client auto-reconnects with backoff; `HELLO` includes last
`manifest_version` → server sends only `MANIFEST_PATCH` + resumes push at
the client's last ack'd cursor (client echoes last-received push ordinal in
`HELLO`). Actions sent while offline are *not* replayed — user retries
(destructive ops need explicit intent).

---

## 7. Frontend: Rust → WASM + wgpu

Toolchain: `wasm32-unknown-unknown`, `wasm-bindgen`, `trunk`; no JS
framework. `index.html` ≈ 60 lines. DOM: the canvas, plus a `beforeunload`
guard while actions pending.

### 7.1 Crate layout

```
web/
  Cargo.toml            # cdylib; opt-level=3, lto="fat", codegen-units=1
  Trunk.toml, index.html
  src/
    lib.rs              # mount(), panic hook, tracing → console
    dom.rs              # canvas/DPR sizing, keyboard/pointer/wheel → mpsc
    net/
      data.rs           # /data socket: frames codec, credits, event fan-out
      bulk.rs           # /bulk socket: manifest assembler, full chunk mux
      proto.rs          # frame header encode/decode (shared layout)
    cache/
      store.rs          # T1 bitmap LRU + T2 bytes LRU + inflight dedupe
      atlas.rs          # 2048² shelf-packer pages, slot alloc/free
      uploader.rs       # per-frame budget, copyExternalImageToTexture glue
    render/
      pipeline.rs       # wgpu init/surface/resize
      quad.rs + tile.wgsl   # instanced tile shader (uv, tint, borders, dim)
      text.rs           # glyphon
      hud.rs            # status/help/counts — same strings as native
    state/
      album.rs          // entries, selection model, focus, zoom — parity w/ native
      layout.rs         // uses shared layout_justified
      crop.rs           // selections/handles + preview
      input.rs          // event→action tables per mode
    preload/
      scorer.rs         // priority scoring (§8.1)
      scheduler.rs      // budgets, velocity tracking, WANT batching
    metrics.rs
```

### 7.2 Render pipeline

* One `wgpu::Surface`; `canvas.clientWidth*devicePixelRatio` sizing,
  reconfigure on resize.
* Tiles = **one instanced draw per atlas page**: instance record
  `{dst: vec4, uv: vec4, tint: vec4, border: vec2}` streamed per frame —
  visible tiles only (< ~500).
* Selected/focused/deleting/placeholder states ride in the instance record
  (border color+width, tint, dim) — no extra painter passes.
* Text via `glyphon` for HUD + focused/hovered filename.
* Filter: `Linear` sampling on 512 px thumbs; optional 2-level mip chain
  only if shimmer shows at deep zoom-out (decide in M2 profiling).

### 7.3 Texture atlas manager

* `2048×2048` RGBA pages, shelf packing; `id → (page, uv)` table.
* Budget `--gpu-mb`-equivalent (~1.5 GB ⇒ ~2.9k 512² thumbs).
* Eviction by distance-from-viewport (not pure LRU — just-scrolled-past
  beats never-shown). Evicted atlas slots keep their decoded bitmap in T1 →
  re-upload ~1 ms, no refetch.
* Full-res crop texture is standalone (≤3840²).

### 7.4 Ingest path (WS → GPU)

```
THUMB/THUMB_BATCH frames ──▶ bytes → T2 store (+indexed per id)
        │
        ▼  decoder stage (cap 4 in flight)
   createImageBitmap(new Blob([jpeg]))  ──▶ bitmap → T1 LRU
        │  (fallback: WASM worker, image::load_from_memory → RGBA bytes)
        ▼  uploader stage (≤8 uploads / rAF)
   copyExternalImageToTexture → atlas slot → resident
```

* `FULL_HEAD`+`FULL_CHUNK` on `/bulk` → assembled `Uint8Array` → same
  decode→upload path but standalone texture.
* Inflight dedupe: `HashMap<u32, Inflight>`; arriving pushes for already-
  resident ids are discarded (sig-checked — a sig mismatch *replaces*:
  server pushed an updated preview after fs-watch).

### 7.5 Input → render loop

* Events → mpsc; each rAF: drain events → state transitions → conditional
  relayout → draw. Draw is synchronous in rAF; the ≤16 ms budget holds
  because all hot paths (select/focus/zoom/scroll) touch only local state +
  resident textures.
* Relayout only on zoom/width/resize/aspect-patch (O(n) — same as native).
* Key maps ported verbatim: album set (arrows/shift/ctrl, Space, Ctrl+A,
  Delete, `c`, Esc, `+`,`-`,`w`,`W`) and crop `KeyboardState` parity
  (Enter/Space/Backspace/Delete/Esc/R/Shift-R/A/H/W/S/P) — one `input.rs`
  table per mode.

### 7.6 Layout & state parity

* `layout_justified` moves to a shared `src/layout.rs` used by album AND
  wasm — identical math by construction.
* Grid never reflows on thumb arrival (aspects come from manifest `w/h`);
  only `MANIFEST_PATCH` aspect updates cause relayout.
* Selection model mirrors `AlbumApp` exactly (`BTreeSet`, focus, anchor,
  range semantics) — with a parity test table (§12).

### 7.7 Crop mode

* `c`/dblclick → `Mode::Crop{id}` → full-res via `/bulk` (usually already
  prefetched §8.5) → `fit_within` framing identical to native.
* `selection.rs` geometry reused; preview (`P`) computes `build_output_image`
  **client-side** in wasm (same crates compile to wasm — zero RTT). Final
  save still round-trips `ACTION{crop}` server-side through the identical
  Rust path.
* `Enter` → optimistic album return + action; `Esc` = embedded-mode parity
  (clear → return to album).

### 7.8 HUD parity

`draw_text_with_bg` reimplemented (rounded dark rect + text): top-left
counts, bottom-left status, bottom-right help line — same strings, colors,
positions as native. Progress line includes `previews n/N` fed by server
`progress` events.

---

## 8. "Extreme intelligent preloading" — push edition

### 8.1 What changed vs. pull model

The server owns global knowledge (what's generated, manifest order, watch
events) — so it *pushes* previews proactively and the client only steers
priority. Client-side scoring still decides *urgency* for `WANT`s and local
decode/upload budgets.

### 8.2 Client priority score (drives `WANT` batches + decode order)

```
score(t) =
    w1 * (1 / (1 + |center_row(t) − viewport_center_row|))   // proximity
  + w2 * is_visible(t)                                       // must-have
  + w3 * in_lookahead(t, velocity, dir)                      // ≤4 rows ahead
  + w4 * is_focused(t) + w5 * is_selected(t)
  + w6 * would_be_crop_target(t)                             // focus±1
  − w7 * failed_count(t)
```

Visible-and-missing tiles always become `WANT` (push can't arrive faster
than a demand). Initial `w2=8,w1=4,w3=3,w4=2,w5=2,w6=4`; self-tuned by
metrics (§8.7).

### 8.3 Server push ordering (per client)

1. `WANT` ids (FIFO within)
2. boosted ids by score (`VIEWPORT`/`FOCUS` hints decay 2× per second)
3. manifest-cursor order (the background "preload ALL" sweep)

…throttled by client credits + global pacing.

### 8.4 Cache tiers

| Tier | Where | Size | Hit→visible |
|---|---|---|---|
| T0 GPU atlas | VRAM | ~1.5 GB | ~0 ms |
| T1 bitmap LRU | wasm | ~512 | ~1 ms |
| T2 bytes LRU | wasm (JPEG) | ~256 MB | ~5–15 ms |
| T3 server RAM | `PreviewStore` LRU | ~2048 | 1 frame |
| T4 disk | `.imagecropper-previews` | ∞ | 1 frame+disk |
| T5 regen | server workers | — | ~50–500 ms |

Every tier miss fills tiers beneath on the way back; T0/T1 eviction never
drops T2+. Pushes land directly in T2 (they're already compressed JPEGs).

### 8.5 Full-res staging

`FOCUS{id}` hint → server moves id into `FullImageStore` queue → client
`GET_FULL`s focus±2 bytes at background priority → decoded to bitmap on
idle → `c` hits the ≤60 ms open-crop budget. Cold opens show the 512 px
preview instantly while chunks stream.

### 8.6 Suspension & eviction

* `document.hidden` → `PAUSE` + drop pending uploads (resume re-primes
  viewport first).
* Fling (|v| spike) → hold all but lookahead-band decodes/uploads; rAF stays
  free.
* T0 evict by distance-from-viewport; T1 LRU 512; T2 LRU by bytes.

### 8.7 Metrics & self-tuning

Rolling 60 s counters (client + optional `stats` event → server):
`prefetch_hit_rate`, `decode_ms` p50/p95, `upload_ms`, `draw_ms`, `ws_rtt`,
`credit_starvation`, `atlas_pressure`, `queue_depths`. Alarms (console):
hit-rate < 95 %, draw > 12 ms, decode p95 > 25 ms. Weights/lookahead radius
auto-adjust toward 98 % hit-rate: growing when bandwidth/decode headroom
exists, shrinking under backlog.

---

## 9. Crop-save roundtrip

```
client: Enter in crop mode
  → optimistic: mode=album, tile dimmed "saving…"
  → /data ACTION{tag,crop:{id,sig,sel,fmt,q}}
server: sig check → build_output_image → Saver → broadcast
        action-done{tag, ok, new_id, new_sig, new_dims}
client: swap manifest entry, WANT new thumb (server already regen'd —
        sig changed), un-dim
errors: sig-mismatch → "stale" → rollback + status line (native parity)
```

Delete = same via `delete` action + `file-gone` broadcast to other clients.

---

## 10. Security & ops

* Loopback bind unless `--serve=0.0.0.0:PORT` explicitly exposes the LAN.
* `HELLO.token` required when `--token` set (random+printed otherwise);
  enforced on both sockets.
* No client-supplied paths — ids only; sig preconditions on all writes.
* WS message size caps: control ≤ 64 KB; bulk chunks fixed 256 KB server
  side; inbound chunk assembly capped at 512 MB.
* Concurrent `GET_FULL` ≤ 4/client; `WANT` batch ≤ 64 ids.
* `tracing` logging; `-v` maps to `debug`.

---

## 11. Compatibility & fallbacks

| Layer | Primary | Fallback |
|---|---|---|
| GPU | WebGPU | WebGL2 (texImage2D bitmap upload) → Canvas2D emergency |
| Transport | WebSocket (any HTTP/1.1) | —none needed; WS is universal. If WS itself is blocked (rare), SSE+POST poll shim = stretch goal only |
| Decode | createImageBitmap | WASM workers (`image` crate) |

Feature-detect `navigator.gpu` at boot; `?force=webgl|canvas2d` overrides.
WS is assumed available (it is, everywhere HTTP/1.1 works — including old
proxies); no long-polling transport fallback in v1.

WASM size target ≤ 3 MB compressed (`wasm-opt -Oz`, slim `image` features —
browser decodes anyway where possible).

---

## 12. Testing plan

| Layer | Tests |
|---|---|
| shared `layout_justified` | existing `album_layout.rs` — identical math guarantee |
| shared preview sig/naming | existing `album_thumbnails.rs` + sig/manifest equality |
| `sort_files` (moved to lib) | size+/size-, exif order with EXIF fixtures |
| `proto` frame codec | golden bytes, round-trips, chunk assembly, malformed-frame rejection |
| PushScheduler | ordering (want>hint>cursor), credit accounting, pause/resume |
| manifest build/patch | version bumps, sig stability, upsert/remove semantics |
| actions (in-process axum) | delete→trash, crop→new file+sig, stale→409-equivalent |
| ws integration | spin server, drive both sockets with a Rust test client: hello→manifest→want→thumbs→action→action-done |
| web `state/` | wasm-bindgen-test: selection parity table, layout parity |
| web `preload/scorer` | deterministic orderings, velocity lookahead |
| e2e | headless Chrome: boot→grid tiles visible→delete→file in trash |
| perf | synthetic 5k dir: time-to-first-tile, scroll hit-rate, credit starvation absence |

Manual QA: Chromium, Safari 26+, FF; LAN `tc qdisc` 20 ms; 10k-dir; tab
hide/resume; WS reconnect mid-session.

---

## 13. Milestones

| M | Content | ~LOC | Exit check |
|---|---|---|---|
| M1 | `--serve` flag wiring: HeadlessArgs on both subcommands, scan→manifest (bulk WS), PreviewStore, asset embedding, `-G` headless paths | 1.8k | test client pulls manifest+thumbs; browser shows grid placeholders; `album -G` warms cache and exits |
| M2 | WebGPU renderer: atlas, instanced tiles, scroll/zoom | 2k | 60 fps on 5k images w/ static test data |
| M3 | /data push pipeline + tiered caches + WANT/scorer | 2.2k | hit-rate ≥95 %, zero gray steady scroll |
| M4 | Selection+keys+delete actions | 1.5k | UX parity with native |
| M5 | Crop mode (client preview + crop action) | 2.5k | pixel-parity crop vs native |
| M6 | fs-watch + manifest-patch + multi-client sync | 1.2k | second tab stays in sync |
| M7 | WebGL2/Canvas2D fallbacks, metrics, docs | 1.6k | matrix QA pass |

≈ 12.7k implementation LOC + ~2–3k tests.

---

## 14. File-by-file breakdown

### Shared/native lib

| File | Change | ~LOC |
|---|---|---|
| `src/scan.rs` (new) | `ScanArgs`, `collect_sorted_files`, `sort_files`, `exif_cmp` (moved out of `main.rs`) | 160 |
| `src/fs_utils.rs` (+`previews`) | signature/filename/dir fns moved from `album::thumbnails`; add `preview_sig_u64` | +90 |
| `src/album/thumbnails.rs` | keep `Thumbnailer`; delegate naming to fs_utils; share `produce` core with `PreviewStore` | refactor ±0 |
| `src/album/layout.rs` → `src/layout.rs` | move verbatim | ±0 |
| `src/main.rs` | `HeadlessArgs` flattened into `CropArgs`+`AlbumArgs`; `--serve`→`run_server(mode)`, `-G`→headless paths; scan/sort logic moved to `scan.rs` | +90 / −120 |

### `src/server/` (new)

| File | Contents | ~LOC |
|---|---|---|
| `mod.rs` | `ServerState`, axum router, `run_server` | 220 |
| `proto.rs` | shared frame codec (also `#[cfg]`-exported for tests) | 250 |
| `manifest.rs` | build/version/patch, dir interning, header-sniff dims | 320 |
| `preview_store.rs` | workers + disk cache + RAM LRU + ready signals | 420 |
| `full_image.rs` | decode/re-encode LRU + concurrency cap | 240 |
| `push.rs` | PushScheduler: wants/boosts/cursor/credits/pacing | 300 |
| `ws_data.rs` | /data endpoint: codec glue, events fan-out | 250 |
| `ws_bulk.rs` | /bulk endpoint: manifest/full chunk senders | 220 |
| `actions.rs` | ActionQueue→Deleter/Saver, sig checks, events | 300 |
| `http.rs` | embedded assets, healthz | 120 |
| `watch.rs` | `notify` → regen/remove/patch events | 160 |

### `web/` (new wasm crate)

| File | ~LOC |
|---|---|
| `lib.rs`, `dom.rs` | 300 |
| `net/proto.rs` (shared frame codec) | 120 |
| `net/data.rs` (mux, credits, events) | 300 |
| `net/bulk.rs` (manifest assembler, full chunks) | 250 |
| `cache/store.rs` | 300 |
| `cache/atlas.rs` | 350 |
| `cache/uploader.rs` | 150 |
| `render/pipeline.rs` | 250 |
| `render/quad.rs`+`tile.wgsl` | 200 |
| `render/text.rs`, `hud.rs` | 300 |
| `state/album.rs` | 500 |
| `state/crop.rs` | 600 |
| `state/input.rs` | 300 |
| `preload/scorer.rs`, `scheduler.rs` | 350 |
| `metrics.rs` | 120 |
| `render/webgl2.rs`, `canvas2d.rs` (M7) | 500 |

### Build/ops

* Cargo **workspace**: `imagecropper` (lib+bin), `imagecropper-web` (cdylib).
* `build.rs`/`xtask`/`just` step: `trunk build --release web` → server
  embeds `web/dist` via `include_bytes!` behind `embed-web` feature (off for
  normal dev builds, on for CI release).
* Deps to add: `axum`, `axum-ws`/`tokio-tungstenite`, `tokio`, `futures`,
  `bincode`, `serde`, `notify`, `lru`, `compact_str`, `webbrowser`
  (auto-open on `--serve`), wasm side:
  `wasm-bindgen`, `wasm-bindgen-futures`, `gloo-net` (WS only), `wgpu`,
  `glyphon`, `js-sys`, `web-sys`, `serde-wasm-bindgen`, `bincode`.

---

## 15. Risks & mitigations

| Risk | Mitigation |
|---|---|
| WS message ordering stalls thumbs behind a big send | bulk on separate socket + 256 KB chunking + interleaved req_ids |
| Server over-pushes on slow link | client credits + PAUSE; pacing cap; push is credit-gated, never free-running |
| Reconnect storms | backoff + resume via `manifest_version`+cursor; actions not replayed |
| `createImageBitmap` quirks (orientation/alpha) | `imageOrientation:'from-image'`; EXIF rotation normalized at preview-gen |
| Browser can't decode AVIF thumbs | previews are JPEG by design |
| 100k-file manifest | bincode + chunking + interned dirs + patch-only updates |
| Two clients conflict | sig precondition → `stale` → client rollback (§9) |
| Hidden tab burns CPU | PAUSE push + suspend decode/upload; resume re-primes viewport |
| `notify` on network mounts | debounce + periodic rescan fallback |
| Giant full-res decode on main thread | decode in worker/`createImageBitmap`; show 512 preview instantly |

---

## 16. Open questions

1. Local companion UI only, or multi-user later? (Assumed local.)
2. ~~Rust/WASM client OK?~~ **Resolved: Rust/WASM + wgpu** (see §A.1).
3. ~~`serve` subcommand vs `album --serve`?~~ **Resolved: no subcommand —
   `-G`/`--no-gui` and `--serve=ADDR:PORT` flags on `crop` and `album`**
   (§5.0 matrix).
4. Marquee selection in the grid: M4 or stretch?
5. Expose native `--resave` batch behavior over WS?
6. Read-only mobile viewing mode (thumbs only) — in scope?
7. Wire `trunk build` into the release job (`embed-web`)?

---

## 17. Definition of done

* [ ] `imagecropper album --serve=127.0.0.1:8737 -r /path` boots; grid
      identical to `album` (layout, overlays, help/status lines).
* [ ] `imagecropper crop --serve=…` boots the web client straight into crop
      mode; Esc drops to the grid.
* [ ] `album -G` pregenerates all previews then exits; `crop -G` batch-
      processes then exits; `-G --serve` skips browser auto-open.
* [ ] All album keys/mouse behaviors identical; parity test passes.
* [ ] `c`/Esc crop round-trip identical to native embedded mode.
* [ ] Delete → `.imagecropper-trash` via action queue; previews purged.
* [ ] `.imagecropper-previews` shared; signature staleness honored both ways.
* [ ] Zero gray tiles in steady 60 fps scroll; hit-rate ≥ 98 % measured.
* [ ] All scan/sort/filter flags honored server-side.
* [ ] All bulk data over the 2 WS channels; HTTP carries only assets.
* [ ] Chrome+Safari+FF pass; WebGL2 fallback works.
* [ ] README: `--serve`/`-G` section, mode matrix, key table, `--token`.

---

## Appendix A — decision records

### A.1 Frontend language: Rust/WASM + wgpu vs TypeScript + WebGPU

**Context:** the latency-critical path (decode via `createImageBitmap`,
upload via `copyExternalImageToTexture`, instanced rasterization) is
browser/GPU-side regardless of client language. The language only owns CPU
orchestration: layout, selection, scoring, instance fill, codecs.

**Decision: Rust/WASM + wgpu.**

Rationale, in order of weight:

1. **Behavioral parity is a maintenance problem, not a one-time cost.**
   The hard requirement is a UI identical to the native app — key tables,
   shift/ctrl selection semantics, embedded-crop Esc behavior, HUD strings,
   layout math, crop preview. In Rust/WASM these are *imports* of the
   existing lib (`layout_justified`, `selection.rs`, `build_output_image`,
   `proto.rs` compiled for both targets) — drift is impossible by
   construction. In TS every behavior is a second implementation requiring
   golden-test pinning and manual re-porting on every future native change.
2. **Determinism under the ≤16 ms input→visible budget.** wasm has linear
   memory and no GC; JS can hold 60 fps with disciplined typed-array/object-
   pool code, but that discipline must be maintained forever. wasm gets it
   by default.
3. **Protocol coherence.** Shared bincode structs between server and client
   make schema changes compile errors on both sides. TS requires codegen or
   hand-mirrored decoders — a runtime bug class.
4. **Client-side crop preview** (`P` key) needs the exact native
   `build_output_image`; `image`/`fast_image_resize` compile to wasm. A TS
   equivalent would not pixel-match.

TS advantages considered and accepted as costs: instant Vite/HMR iteration
vs. `trunk` rebuilds; DevTools/WebGPU-inspector debugging vs. wasm traces;
~150–400 KB bundle vs. ~1–3 MB compressed wasm; first-class Web API access
vs. `web-sys` boundary calls (~100 ns–1 µs each, negligible at our call
volume).

**Condition / fallback:** M1 must include a 1-day spike — minimal wgpu-wasm
quad renderer using `Queue::copy_external_image_to_texture` fed by pushed
JPEG frames. If upload ergonomics in wgpu 30 on wasm are broken, fall back
in order: (a) `ImageBitmap`→`OffscreenCanvas`→`texImage2D`-style path or
`queue.write_texture` of decoded RGBA (~1–3 ms/upload extra, acceptable),
(b) hybrid TS-render + wasm-logic crate (parity for layout/selection/proto,
reimplemented render), (c) full TS rewrite only as last resort, gated by
golden parity tests on the input/selection/layout tables.
