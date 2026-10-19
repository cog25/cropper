//! Shared data types, all coordinates in PDF points, display space
//! (top-left origin, y-down) so that rendered pixels and krilla surfaces
//! share one coordinate system.

use serde::{Deserialize, Serialize};

/// A rectangle in point units (display space).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CropRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl CropRect {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self { x, y, width, height }
    }

    pub fn full(width: f32, height: f32) -> Self {
        Self { x: 0.0, y: 0.0, width, height }
    }

    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }

    /// Clamp into the page rectangle `[0, w] x [0, h]`, keeping at least a
    /// 2pt sliver so the resulting page is never degenerate.
    pub fn clamped(&self, page_w: f32, page_h: f32) -> Self {
        let x = self.x.max(0.0).min((page_w - 2.0).max(0.0));
        let y = self.y.max(0.0).min((page_h - 2.0).max(0.0));
        let width = self.width.min(page_w - x).max(2.0);
        let height = self.height.min(page_h - y).max(2.0);
        Self { x, y, width, height }
    }

    /// Expand by `pad` on all sides, clamped to the page.
    pub fn padded(&self, pad: f32, page_w: f32, page_h: f32) -> Self {
        CropRect::new(
            self.x - pad,
            self.y - pad,
            self.width + 2.0 * pad,
            self.height + 2.0 * pad,
        )
        .clamped(page_w, page_h)
    }

    /// Smallest rectangle covering both.
    pub fn union(&self, other: &CropRect) -> CropRect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        CropRect::new(
            x,
            y,
            self.right().max(other.right()) - x,
            self.bottom().max(other.bottom()) - y,
        )
    }
}

/// Options controlling ink detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DetectOptions {
    /// Luma (0..=255) below which a pixel counts as ink.
    pub threshold: u8,
    /// Minimum ink pixels a row/column must contain to count as content.
    pub min_ink: u32,
    /// Band at every page edge (mm) ignored by the detector, so thin
    /// scanner edge lines don't stretch the crop.
    pub edge_mm: f32,
}

impl Default for DetectOptions {
    fn default() -> Self {
        Self { threshold: 200, min_ink: 3, edge_mm: 0.0 }
    }
}

/// Result of analyzing one page.
#[derive(Debug, Clone, Serialize)]
pub struct PageAnalysis {
    /// Page size in points (display orientation).
    pub page_width: f32,
    pub page_height: f32,
    /// Detected content rect in points (before padding).
    pub crop: CropRect,
    /// Share of pixels inside the content rect that are ink.
    pub ink_ratio: f32,
    /// True when (almost) no content was found; `crop` is the full page.
    pub blank: bool,
}

/// Standard paper sizes in points (portrait).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaperSize {
    A3,
    A4,
    A5,
    B4Jis,
    B5Jis,
    Letter,
    Legal,
}

impl PaperSize {
    /// Width and height in millimeters (portrait).
    pub fn dims_mm(self) -> (f32, f32) {
        match self {
            Self::A3 => (297.0, 420.0),
            Self::A4 => (210.0, 297.0),
            Self::A5 => (148.0, 210.0),
            Self::B4Jis => (257.0, 364.0),
            Self::B5Jis => (182.0, 257.0),
            Self::Letter => (215.9, 279.4),
            Self::Legal => (215.9, 355.6),
        }
    }

    /// Width and height in points (portrait).
    pub fn dims_pt(self) -> (f32, f32) {
        let (w, h) = self.dims_mm();
        (w * 72.0 / 25.4, h * 72.0 / 25.4)
    }

    /// Short display name.
    pub fn label(self) -> &'static str {
        match self {
            Self::A3 => "A3",
            Self::A4 => "A4",
            Self::A5 => "A5",
            Self::B4Jis => "JIS B4",
            Self::B5Jis => "JIS B5",
            Self::Letter => "Letter",
            Self::Legal => "Legal",
        }
    }

    /// Parse a name like "A4" or "b4" (CLI convenience).
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "A3" => Some(Self::A3),
            "A4" => Some(Self::A4),
            "A5" => Some(Self::A5),
            "B4" => Some(Self::B4Jis),
            "B5" => Some(Self::B5Jis),
            "LETTER" => Some(Self::Letter),
            "LEGAL" => Some(Self::Legal),
            _ => None,
        }
    }
}

/// Final per-page geometry for the exporter: which rect of the source page to
/// show, on an output page of the given size, after a uniform scale.
///
/// The mapping is `out = scale * (src - crop.min) + offset`, all in points,
/// so `scale = 1, offset = 0` draws the crop 1:1 at the page origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlacedCrop {
    pub crop: CropRect,
    /// Output (MediaBox) page size in points.
    pub page_width: f32,
    pub page_height: f32,
    /// Uniform scale applied after cropping.
    pub scale: f32,
    /// Where the scaled crop content starts on the output page.
    pub offset_x: f32,
    pub offset_y: f32,
}

impl PlacedCrop {
    /// Crop drawn 1:1, top-left aligned, page sized to the crop.
    pub fn on_page(crop: CropRect) -> Self {
        let crop = crop;
        Self {
            crop,
            page_width: crop.width,
            page_height: crop.height,
            scale: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
        }
    }
}
