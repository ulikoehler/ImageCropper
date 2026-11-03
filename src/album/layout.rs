use eframe::egui::{self, Rect, Vec2};

/// A single row of the justified layout.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutRow {
    /// Index of the first item in the row.
    pub start: usize,
    /// Index one past the last item in the row.
    pub end: usize,
    /// Bounds of the row (including the implicit trailing gap).
    pub rect: Rect,
}

/// Result of a space-filling (justified) tiling pass: every completed row is
/// scaled so its tiles fill the available width exactly.
#[derive(Debug, Clone)]
pub struct JustifiedLayout {
    /// Tile rect for every item, indexed like the input aspect slice.
    pub tiles: Vec<Rect>,
    /// Row boundaries in order, for keyboard navigation.
    pub rows: Vec<LayoutRow>,
    /// Total size of the laid-out content.
    pub total_size: Vec2,
}

impl JustifiedLayout {
    /// Find which row contains the given item index.
    pub fn row_of(&self, index: usize) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| index >= row.start && index < row.end)
    }
}

/// Lay out items with the given aspect ratios (width / height) into rows that
/// fill `width` exactly. `target_row_height` is the desired height of a row at
/// zoom 1.0; `gap` is inserted between tiles and rows.
///
/// Rows that are full are stretched to exactly fill the width (classic
/// justified / Google-Photos style). The trailing incomplete row is left at
/// the target height so its tiles are not blown up.
pub fn layout_justified(
    aspects: &[f32],
    width: f32,
    target_row_height: f32,
    gap: f32,
) -> JustifiedLayout {
    let n = aspects.len();
    let mut tiles = vec![Rect::NOTHING; n];
    let mut rows = Vec::new();

    if n == 0 || width <= 0.0 || target_row_height <= 0.0 {
        return JustifiedLayout {
            tiles,
            rows,
            total_size: Vec2::ZERO,
        };
    }

    let mut y = 0.0f32;
    let mut i = 0usize;

    while i < n {
        let row_start = i;
        let mut aspect_sum = 0.0f32;

        // Greedily add items until the row overflows at target height.
        while i < n {
            let aspect = aspects[i].max(0.01);
            let projected_width =
                (aspect_sum + aspect) * target_row_height + (i - row_start) as f32 * gap;
            if projected_width > width && i > row_start {
                break;
            }
            aspect_sum += aspect;
            i += 1;
        }

        let count = i - row_start;
        let inner_gaps = (count - 1) as f32 * gap;
        let complete_row = i < n;
        // Completed rows fill the width exactly; the last row keeps the
        // target height (capped so very wide tiles can't overflow the width).
        let row_height = if complete_row && aspect_sum > 0.0 {
            ((width - inner_gaps) / aspect_sum).min(target_row_height * 3.0)
        } else {
            target_row_height.min(((width - inner_gaps) / aspect_sum.max(0.01)).max(1.0))
        };

        let mut x = 0.0f32;
        for j in row_start..i {
            let w = aspects[j].max(0.01) * row_height;
            tiles[j] = Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, row_height));
            x += w + gap;
        }

        rows.push(LayoutRow {
            start: row_start,
            end: i,
            rect: Rect::from_min_size(egui::pos2(0.0, y), egui::vec2(width, row_height)),
        });

        y += row_height + gap;
    }

    JustifiedLayout {
        tiles,
        rows,
        total_size: egui::vec2(width, (y - gap).max(0.0)),
    }
}
