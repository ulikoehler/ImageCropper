# ImageCropper

A fast, fullscreen image cropping tool written in Rust using `egui` and `wgpu`. Designed for efficient workflows involving large datasets of images.

## Features

*   **Fullscreen Interface**: Maximizes screen real estate for image viewing.
*   **Efficient Workflow**: Keyboard-driven navigation and operations.
*   **Non-Destructive (by default)**: Original images are moved to a backup folder (`.imagecropper-originals`) instead of being overwritten or deleted.
*   **AVIF Output**: Automatically converts and saves cropped images as AVIF for high efficiency.
*   **Multiple Selections**: Crop multiple regions from a single image at once.
*   **Background Processing**: Saving and conversion happens in the background to keep the UI responsive.
*   **Preloading**: Preloads next/previous images for instant navigation.

## Installation

Ensure you have [Rust installed](https://rustup.rs/).

```bash
cargo install --path .
```

## Usage

```bash
imagecropper [OPTIONS] <DIRECTORY>
```

### Options

*   `-q, --quality <QUALITY>`: Set the output AVIF quality (1-100). Default is **60**.
*   `--resave`: Automatically convert images to AVIF when navigating away from them, even if no crop was performed. Useful for batch converting a folder.
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

## Workflow

1.  Run the tool on a directory of images.
2.  Navigate through images using **Space**.
3.  If an image needs cropping, select the region(s) and press **Enter**.
    *   The original is backed up.
    *   The crop is saved as an AVIF.
    *   The tool advances to the next image.
4.  If an image is bad, press **Delete** to move it to trash.
5.  If `--resave` is on, simply pressing **Space** on a non-AVIF image will convert it to AVIF in the background.

## Output

*   **Cropped Images**: Saved in the same directory with the `.avif` extension.
*   **Originals**: Moved to `.imagecropper-originals/` in the working directory.
*   **Trash**: Moved to `.imagecropper-trash/` in the working directory.
