//! Margin detection on rendered grayscale pixels.

use crate::render::PageRaster;
use crate::types::{CropRect, DetectOptions};

/// Ink bounding box in pixel coordinates (right/bottom exclusive).
#[derive(Debug, Clone, Copy)]
pub struct PixelBBox {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

/// Analyze a rendered page and return the ink bounding box in pixels plus a
/// few metrics used by the UI.
pub fn analyze(raster: &PageRaster, opts: &DetectOptions) -> (Option<PixelBBox>, f32, u8) {
    let w = raster.width as usize;
    let h = raster.height as usize;
    let data = &raster.luma;

    // Estimate the paper brightness as the 95th percentile of luma. Pure
    // scans sit near 255; darker paper yields a lower background.
    let mut hist = [0u32; 256];
    for &v in data {
        hist[v as usize] += 1;
    }
    let total = (w * h).max(1) as u64;
    let mut acc = 0u64;
    let mut bg = 255u8;
    for (bin, &count) in hist.iter().enumerate() {
        acc += count as u64;
        if acc * 100 >= total * 95 {
            bg = bin as u8;
            break;
        }
    }

    // Never cut into the paper tone itself: keep a small gap between the
    // threshold and the background level.
    let threshold = opts.threshold.min(bg.saturating_sub(8)).max(1);
    let min_ink = opts.min_ink.max(1) as usize;

    // Ink profiles.
    let mut row_counts = vec![0u32; h];
    let mut col_counts = vec![0u32; w];
    let mut ink_total: u64 = 0;
    let mm_to_px = (72.0 / 25.4) * w as f32 / raster.page_width.max(1.0);
    let edge = ((opts.edge_mm.max(0.0) * mm_to_px).round() as usize).min(w.min(h) / 4);
    for (y, row) in data.chunks_exact(w).enumerate() {
        if y < edge || y >= h - edge {
            continue;
        }
        let mut count = 0u32;
        for (x, &v) in row.iter().enumerate().take(w - edge).skip(edge) {
            if v < threshold {
                count += 1;
                col_counts[x] += 1;
            }
        }
        row_counts[y] = count;
        ink_total += count as u64;
    }

    let bbox = find_bbox(
        &row_counts,
        &col_counts,
        min_ink,
        w,
        h,
    );

    let ink_ratio = if let Some(b) = &bbox {
        let area = ((b.x1 - b.x0) * (b.y1 - b.y0)).max(1) as f64;
        (ink_total as f64 / area) as f32
    } else {
        0.0
    };

    (bbox, ink_ratio, threshold)
}

/// Find the ink bounding box from row/column profiles, skipping scanner
/// border bands and isolated noise pixels.
fn find_bbox(
    row_counts: &[u32],
    col_counts: &[u32],
    min_ink: usize,
    w: usize,
    h: usize,
) -> Option<PixelBBox> {
    if row_counts.is_empty() || col_counts.is_empty() {
        return None;
    }

    // Scanner border artifacts: an (almost) solid dark line hugging the
    // paper edge. They live in a narrow band at the outside; a row is
    // "solid" when more than half its pixels are ink.
    let band_y = (h as f32 * 0.012).ceil() as usize;
    let band_x = (w as f32 * 0.012).ceil() as usize;
    let solid_row = (w as u32 / 2).max(1);
    let solid_col = (h as u32 / 2).max(1);

    let mut y0 = {
        let mut y = 0usize;
        while y < band_y.min(h) && row_counts[y] >= solid_row {
            y += 1;
        }
        y
    };
    let mut y1 = {
        let mut y = h;
        while y > h.saturating_sub(band_y) && row_counts[y - 1] >= solid_row {
            y -= 1;
        }
        y
    };
    let mut x0 = {
        let mut x = 0usize;
        while x < band_x.min(w) && col_counts[x] >= solid_col {
            x += 1;
        }
        x
    };
    let mut x1 = {
        let mut x = w;
        while x > w.saturating_sub(band_x) && col_counts[x - 1] >= solid_col {
            x -= 1;
        }
        x
    };

    // Content bounds: first/last row/col with at least `min_ink` pixels.
    if let Some(y) = (y0..y1).find(|&y| row_counts[y] as usize >= min_ink) {
        y0 = y;
    } else {
        return None;
    }
    if let Some(y) = (y0..y1).rev().find(|&y| row_counts[y] as usize >= min_ink) {
        y1 = y + 1;
    } else {
        return None;
    }
    if let Some(x) = (x0..x1).find(|&x| col_counts[x] as usize >= min_ink) {
        x0 = x;
    } else {
        return None;
    }
    if let Some(x) = (x0..x1).rev().find(|&x| col_counts[x] as usize >= min_ink) {
        x1 = x + 1;
    } else {
        return None;
    }

    if x1 <= x0 || y1 <= y0 {
        return None;
    }

    Some(PixelBBox {
        x0: x0 as u32,
        y0: y0 as u32,
        x1: x1 as u32,
        y1: y1 as u32,
    })
}

/// Convenience: analyze + convert to a `CropRect` in points.
pub fn analyze_to_crop(
    raster: &PageRaster,
    opts: &DetectOptions,
) -> (CropRect, f32, bool, u8) {
    let (bbox, ink_ratio, threshold) = analyze(raster, opts);
    let sx = raster.width as f32 / raster.page_width;
    let sy = raster.height as f32 / raster.page_height;
    let crop = match bbox {
        Some(b) => CropRect::new(
            b.x0 as f32 / sx,
            b.y0 as f32 / sy,
            (b.x1 - b.x0) as f32 / sx,
            (b.y1 - b.y0) as f32 / sy,
        ),
        None => CropRect::full(raster.page_width, raster.page_height),
    };
    (crop, ink_ratio, bbox.is_none(), threshold)
}
