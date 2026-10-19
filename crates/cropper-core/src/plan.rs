//! Turn per-page detection results into final placements: source crop rect +
//! output page size + scale/offset (padding, uniform mode, per-page enable
//! flags, fixed-paper targets), all in points.

use crate::types::{CropRect, PlacedCrop};

/// Whether each page keeps its own detected crop or all pages share one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CropMode {
    PerPage,
    Uniform,
}

/// Output page sizing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TargetSize {
    /// The output page is exactly the (padded) crop, content at 1:1.
    Content,
    /// Fixed paper size in points. Content is uniformly scaled to fit inside
    /// symmetric `pad_mm` margins and placed per `PlanOptions::align`. The paper orientation
    /// follows the content (portrait content on portrait paper, etc.).
    Paper { width: f32, height: f32 },
}

#[derive(Debug, Clone, Copy)]
pub struct PlanOptions {
    /// Margin in millimeters. Content mode: kept around the crop. Paper
    /// mode: guaranteed margin between content and paper edge.
    pub pad_mm: f32,
    pub mode: CropMode,
    pub target: TargetSize,
    /// Where the content sits inside the paper margins, per axis:
    /// 0.0 = left/top, 0.5 = center, 1.0 = right/bottom. Paper mode only.
    pub align: (f32, f32),
}

impl Default for PlanOptions {
    fn default() -> Self {
        Self { pad_mm: 3.0, mode: CropMode::PerPage, target: TargetSize::Content, align: (0.5, 0.5) }
    }
}

/// `detected` = per-page crops from the detector (`None` = blank or not
/// analyzed), `enabled` = per-page participate flags. Excluded pages produce
/// `None` and are exported untouched; blank pages too, unless a fixed paper
/// size is targeted.
pub fn plan_crops(
    dims: &[(f32, f32)],
    detected: &[Option<CropRect>],
    enabled: &[bool],
    options: &PlanOptions,
) -> Vec<Option<PlacedCrop>> {
    let pad = options.pad_mm * (72.0 / 25.4);

    let mut uniform: Option<CropRect> = None;
    if options.mode == CropMode::Uniform {
        for crop in detected.iter().flatten() {
            uniform = Some(match uniform {
                None => *crop,
                Some(u) => u.union(crop),
            });
        }
    }

    dims.iter()
        .enumerate()
        .map(|(index, &(page_width, page_height))| {
            if !enabled.get(index).copied().unwrap_or(true) {
                return None;
            }
            let detected = match detected.get(index).copied().flatten() {
                Some(c) => c,
                // Blank/unanalyzed: untouched in Content mode, but with a
                // fixed paper the whole page is fit onto it so every output
                // page has the same size.
                None if matches!(options.target, TargetSize::Paper { .. }) => {
                    CropRect::full(page_width, page_height)
                }
                None => return None,
            };
            let crop = match options.mode {
                CropMode::PerPage => detected,
                CropMode::Uniform => uniform
                    .expect("uniform mode requires at least one detected page")
                    .clamped(page_width, page_height),
            };
            let crop = crop.padded(pad, page_width, page_height);

            Some(match options.target {
                TargetSize::Content => PlacedCrop::on_page(crop),
                TargetSize::Paper { width, height } => {
                    // Match orientation to the content's aspect ratio.
                    let (pw, ph) = if (crop.width > crop.height) == (width > height) {
                        (width, height)
                    } else {
                        (height, width)
                    };
                    let avail_w = (pw - 2.0 * pad).max(4.0);
                    let avail_h = (ph - 2.0 * pad).max(4.0);
                    let scale = ((avail_w / crop.width).min(avail_h / crop.height)).max(0.01);
                    PlacedCrop {
                        crop,
                        page_width: pw,
                        page_height: ph,
                        scale,
                        offset_x: pad + (avail_w - crop.width * scale).max(0.0) * options.align.0,
                        offset_y: pad + (avail_h - crop.height * scale).max(0.0) * options.align.1,
                    }
                }
            })
        })
        .collect()
}
