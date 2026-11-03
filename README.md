# ImageCropper

A fast, fullscreen image cropping tool written in Rust using `egui` and `wgpu`. Designed for efficient workflows involving large datasets of images.

## Quick overview

## Installation

Global installation:

```sh
cargo install imagecropper
```

and ensure `~/.cargo/bin` is in your `PATH`. Then, you can just run

```sh
imagecropper crop <DIRECTORY>
```

### Simple cropping

```sh
imagecropper crop test-images
```

Drag with your mouse to select the crop area, then press **Enter** to save the cropped image and move to the next one. Use **Space** to skip images and **Delete** to move bad images to trash.

![ImageCropper screenshot](docs/Imagecropper%20Screenshot.avif)

when pressing **Enter**, it will save this image:

![Cropped image example](docs/Cropped%20Testimage.avif)

### Multi-selection cropping (Multicropping)

You can create multiple selections by holding **Ctrl** while dragging. Press **Enter** to crop all selected areas from the current image and assemble them into a single image.

![Multicrop selection](docs/Imagecropper%20Multicrop.avif)

results in

![Multicrop result](docs/Multicrop%20Result.avif)

### Output format

By default, cropped images are saved as AVIF files for high efficiency - **Saving AVIF files takes a LONG time (minutes!) but the TINY filesize despite HIGH QUALITY is impressive**. You can adjust the quality using the `-q` option, or choose a different output format using `-f/--format`.

```sh
imagecropper crop -f png test-images
```

### Recursive directory scanning

By default, ImageCropper scans only the files in the top-level directory you provide. If you want to include images inside subdirectories as well, use `-r/--recursive` to enable recursive scanning.

### Image processing order

You can control the order in which images are processed using the `-o/--order` option. By default, images are processed in filename order. You can invert the sorting using `-i/--inverse-order`.

Available orderings: `filename`, `randomize`, `modified` (last modified time), `size`/`size+` (smallest first), `size-` (largest first), and `exif`/`exif+`/`exif-` (EXIF capture time, oldest/newest first; files without EXIF metadata sort last).

To select images in random order:

```sh
imagecropper crop -o randomize test-images
```

or by last modified time:

```sh
imagecropper crop -o modified test-images
```

or by EXIF capture time, newest photos first:

```sh
imagecropper crop -o exif- test-images
```

### Resave unchanged images?

You can use the `--resave` option to automatically convert images to AVIF when navigating away from them, even if no crop was performed. This is useful for batch converting a folder of images.

```sh
imagecropper crop --resave test-images
```

## Album mode

`imagecropper album` opens a fullscreen, space-filling tiled view of every image in the given paths — each completed row is scaled to fill the window width exactly (justified layout).

```sh
imagecropper album test-images
imagecropper album -r -o exif- test-images
```

All previews are actively generated in the background for **all** images (prioritizing what is on screen) and persisted in `.imagecropper-previews/` next to each image, so subsequent runs start instantly. A fullscreen-size (≤1920px) JPEG tier is generated alongside and persisted in `~/.cache/imagecropper/f/…` (honors `$XDG_CACHE_HOME`) for instant hold-**f** fullscreen views. The preview file names encode the source file's size and modification time — change a file and its preview is regenerated automatically; stale previews are cleaned up. Preview worker counts self-tune to the storage device (few/sequential on HDDs and network mounts, up to `-j` on SSDs).

JPEGs decode on the **GPU** via a pure-Rust wgpu/WGSL compute decoder — bit-exact vs libjpeg-turbo and roughly 10× faster than the CPU path for large images. Baseline and progressive files both work; files without restart markers (most camera JPEGs, `DRI=0`) get a host survey pass that synthesizes virtual restart intervals, so serial scans decode in parallel too. EXIF orientation is applied on the GPU. Anything unsupported (non-8-bit, arithmetic-coded, >3 components) or a missing GPU transparently falls back to the fast CPU decoder. Set `IC_NO_GPUJPEG=1` to disable.

JPEG **encoding** runs on the GPU too: a WGSL port of the restart-parallel compute encoder (one work item per MCU — level shift, DCT, quantization, Huffman with 0xFF stuffing; the host only assembles JFIF markers and RST segments). Preview generation for JPEG sources is therefore decode → resize → encode without pixels ever leaving the device — only the finished `.jpg` bytes cross back. GPU jobs are fully pipelined: every call waits only on its own queue submissions, so an arbitrary number of images can be in flight at once across the preview worker pool.

Directory listing runs in the background and streams results in filesystem order — tiles appear and previews generate while the scan is still in flight, which matters on slow SMB/network mounts. The `-o` ordering is applied once the scan completes.

Album mode accepts the same scanning options as `crop`: `-r/--recursive`, `--whitelist`, `--blacklist`, `--filter-syntax`, `-o/--order`, `-i/--inverse-order`, `-j/--parallel`, `-d/--dry-run` and `-v/--verbose`.

Use `-m/--move DIR` to designate a destination directory (created if missing) — pressing **m** then moves all selected images there through a background queue, like deletion but non-destructive:

```sh
imagecropper album -m ~/keep -r ~/incoming-photos
```

### Album controls

*   **Mouse Click**: Select a single image.
*   **Ctrl + Click**: Toggle an image's selection.
*   **Shift + Click**: Select the range from the anchor to the clicked image.
*   **Mouse Drag**: Select a contiguous **sequence** of images (list order from the drag-start tile to the cursor tile — `--mouse-select-mode=sequence`, the default). `--mouse-select-mode=rubber-band` instead selects every tile the drag rectangle intersects. Shift-drag adds to the selection, Ctrl/Cmd-drag toggles it.
*   **Double Click / c**: Open the image in crop mode. **Esc** leaves crop mode and returns to the album.
*   **Arrow Keys**: Move the focused tile (plain arrows select it, **Shift** extends the selection, **Ctrl** moves without changing the selection).
*   **Space**: Toggle the focused image's selection.
*   **Ctrl + A**: Select all images.
*   **Delete**: Selected tiles vanish immediately; the actual move to `.imagecropper-trash` runs on a background deletion queue whose worker count tracks the preview pool's autotuned concurrency (same storage, same optimal parallelism). The top line shows the live queue (`deleting N`, `moving N`).
*   **m**: Same flow for the `-m/--move` directory — tiles vanish at keypress, the move completes in the background. (No-op with a status hint when `-m` was not given.)
*   **f (hold)**: Loupe the image under the mouse fullscreen (falls back to the focused image when hovering nothing) using the preloaded ≤1920px preview; release to return to the grid. Moving the mouse while held switches the loupe to the newly hovered tile.
*   **+ / -**: Fine-grained zoom of the tile size.
*   **W / w** (Shift+W / w): Grow / shrink the viewer's width — the grid narrows into a centered band, so repeated `w` presses make it less and less wide.
*   **Esc**: Clear the selection; with no selection, exit.
*   **Ctrl-C**: Stop all preview generation immediately and exit — queued deletions/moves are preferentially finished first (the window shows the remaining count). On exit the CLI prints `Total deleted: N file(s) (size)` and `Total moved: N file(s) (size)`.

## Web UI (`--serve` / `-G`)

Both subcommands can serve a browser UI over two WebSocket channels instead of opening a native window:

```sh
imagecropper album --serve=0.0.0.0:12345 -r /photos   # album grid in the browser
imagecropper crop  --serve=127.0.0.1:8737 test-images # web crop session
imagecropper album --serve=127.0.0.1:8737 -G /photos  # serve, no browser auto-open
imagecropper album -G /photos                          # headless: pregenerate previews
imagecropper crop  -G --resave /photos                 # headless: batch resave
```

*   `/data` carries controls, events and pushed thumbnails; `/bulk` carries the manifest and full/screen-size images. Multiple clients can connect simultaneously — found/changed/deleted files arrive as differential `manifest-patch` updates with stable positional ids (no reloads).
*   Files stream into the manifest as the background scan discovers them; clients show partial lists immediately.
*   The web client mirrors the native album controls (click/ctrl/shift selection, arrows, `+/-`, `w/W`, `Delete`, `m`, hold-`f` fullscreen).
*   `-v/--verbose` prints scan progress, preview cache hits/generation and actions to stderr.
*   Auth defaults to `--auth=token` — a random token generated per run; the printed/opened URL already contains `?token=…` (required for the WS upgrade on both sockets). `--auth=none` disables auth entirely; `--auth=password -p <SECRET>` (or `-p` alone) pins a fixed password — the client prompts for it when the URL lacks `?token=`.

## Features

*   **Fullscreen Interface**: Maximizes screen real estate for image viewing.
*   **Efficient Workflow**: Keyboard-driven navigation and operations.
*   **Non-Destructive (by default)**: Original images are moved to a backup folder (`.imagecropper-originals`) instead of being overwritten or deleted.
*   **AVIF Output**: Automatically converts and saves cropped images as AVIF for high efficiency.
*   **Multiple Selections**: Crop multiple regions from a single image at once.
*   **Background Processing**: Saving and conversion happens in the background to keep the UI responsive.
*   **Preloading**: Preloads next/previous images for instant navigation.

## Continuous Integration

GitHub Actions builds release binaries for Linux and Windows on every push to `master`, on pull requests, and on manual workflow runs.

When you push a version tag like `v0.3.1`, the workflow also attaches packaged binaries to the corresponding GitHub release:

*   `imagecropper-linux-x86_64.tar.gz`
*   `imagecropper-windows-x86_64.zip`

## Usage

ImageCropper is organized into subcommands. The interactive cropping workflow is available via `crop`:

```bash
imagecropper crop [OPTIONS] <PATHS>...
```

### Options for `crop`

*   `-q, --quality <QUALITY>`: Set the output AVIF quality (1-100). Default is **70** which is a conservative choice. Most photos will do fine with q=30 even if you are watching them on a TV. Checkout [MisterAVIF](https://github.com/ulikoehler/MisterAVIF) for a tool to determine the correct quality level for *your* images.
*   `-o, --order <ORDER>`: Image order: `filename` (default), `randomize`, `modified`, `size`/`size+`, `size-`, `exif`/`exif+`, `exif-`.
*   `-r, --recursive`: Recursively scan subdirectories for images. Disabled by default.
*   `--resave`: Automatically convert images to AVIF when navigating away from them, even if no crop was performed. Useful for batch converting a folder.
*   `--report-sizes`: When enabled, show the original and new file sizes (human-readable: KB/MB) and the percentage of the new file after background save/backup operations complete.
*   `--dry-run`: Simulate operations without moving or writing files.

### Controls

*   **Mouse Drag**: Create a selection.
*   **Ctrl + Mouse Drag**: Create additional selections.
*   **Drag Handles/Corners**: Resize the active selection.
*   **Arrow Keys**: Move all selections.
*   **Enter**: Crop the selected area(s) and save. Moves to the next image.
*   **Space**: Skip to the next image (triggers auto-resave if enabled).
*   **Backspace**: Go to the previous image.
*   **Delete**: Move the current image to the trash folder (`.imagecropper-trash`).
*   **Esc**: Clear current selection. If no selection, exit the application.
*   **R**: Rotate the current image 90° clockwise.
*   **Shift + R**: Rotate the current image 90° counter-clockwise.

    Note: rotating the image clears any existing selections because selection coordinates are image-space specific.

## Workflow

1.  Run the tool on a directory of images.
2.  Navigate through images using **Space**.
3.  If an image needs cropping, select the region(s) and press **Enter**.
    *   The original is backed up.
    *   The crop is saved as an AVIF.
    *   The tool advances to the next image.
4.  If an image is bad, press **Delete** to move it to trash. Deleted file sizes are tallied separately and shown in the exit summary.
5.  If `--resave` is on, simply pressing **Space** on a non-AVIF image will convert it to AVIF in the background.

## Output

*   **Cropped Images**: Saved in the same directory with the `.avif` extension.
*   **Originals**: Moved to `.imagecropper-originals/` in the directory where the image is located.
*   **Trash**: Moved to `.imagecropper-trash/` in the directory where the image is located.

## License

Apache-2.0 License. See [`LICENSE`](LICENSE) file for details.

The test images are [Uli Köhler's](https://github.com/ulikoehler) work and are hereby released into the public domain (CC0 1.0 Universal).