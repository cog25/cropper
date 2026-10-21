//! Shared document pipeline: open → render/cache → detect → export.
//! Used by both the desktop GUI and the wasm bindings.

use std::collections::HashMap;
use std::sync::Arc;

use hayro::hayro_syntax::Pdf;

use crate::detect;
use crate::render::PageRaster;
use crate::types::{CropRect, DetectOptions};

/// One open PDF document with cached page rasters.
pub struct Session {
    source: Vec<u8>,
    pdf: Arc<Pdf>,
    dims: Vec<(f32, f32)>,
    /// (max_px, raster) per page index, so threshold changes re-detect
    /// without re-rendering.
    rasters: HashMap<usize, (u32, PageRaster)>,
}

impl Session {
    pub fn new(source: Vec<u8>) -> Result<Self, String> {
        let pdf = Arc::new(
            Pdf::new(source.clone()).map_err(|_| "PDF 파일을 해석할 수 없습니다".to_string())?,
        );
        let dims = pdf.pages().iter().map(|p| p.render_dimensions()).collect();
        Ok(Self { source, pdf, dims, rasters: HashMap::new() })
    }

    pub fn page_count(&self) -> usize {
        self.dims.len()
    }

    /// Page size in points, display orientation.
    pub fn page_size(&self, index: usize) -> Result<(f32, f32), String> {
        self.dims
            .get(index)
            .copied()
            .ok_or_else(|| format!("페이지 번호가 범위를 벗어났습니다: {index}"))
    }

    /// Shared handle to the parsed document, so worker threads can render
    /// pages in parallel (see [`render_page`]).
    pub fn pdf(&self) -> Arc<Pdf> {
        Arc::clone(&self.pdf)
    }

    /// Store a raster rendered elsewhere (worker thread) so `analyze` finds
    /// it cached.
    pub fn insert_raster(&mut self, index: usize, max_px: u32, raster: PageRaster) {
        self.rasters.insert(index, (max_px, raster));
    }

    /// Analyze one page, rendering it first when needed (at `max_px` long
    /// side). Returns the detected crop, ink ratio, blank flag and the
    /// effective threshold.
    pub fn analyze(
        &mut self,
        index: usize,
        max_px: u32,
        options: &DetectOptions,
    ) -> Result<(CropRect, f32, bool, u8), String> {
        self.page_size(index)?; // validate range; raster carries its own page dims

        let needs_render = match self.rasters.get(&index) {
            Some((cached_px, _)) => {
                if *cached_px != max_px {
                    self.rasters.remove(&index);
                    true
                } else {
                    false
                }
            }
            None => true,
        };
        if needs_render {
            let page = &self.pdf.pages()[index];
            let raster = PageRaster::render(page, max_px);
            self.rasters.insert(index, (max_px, raster));
        }

        let raster = &self.rasters[&index].1;
        let (crop, ink_ratio, blank, threshold) = detect::analyze_to_crop(raster, options);
        Ok((crop, ink_ratio, blank, threshold))
    }

    /// Render a page at `max_px` (long side) without touching the detection
    /// cache; used for sharp zoomed previews.
    pub fn render_detail(&self, index: usize, max_px: u32) -> Result<PageRaster, String> {
        self.page_size(index)?;
        Ok(PageRaster::render(&self.pdf.pages()[index], max_px))
    }

    /// Cached raster for a page, if it has been analyzed.
    pub fn raster(&self, index: usize) -> Option<&PageRaster> {
        self.rasters.get(&index).map(|(_, r)| r)
    }

    /// Build the cropped PDF from final placements.
    pub fn export(&self, placements: &[Option<crate::types::PlacedCrop>]) -> Result<Vec<u8>, String> {
        crate::export::export_cropped(&self.source, placements)
    }
}

/// Render one page of `pdf` without a `Session`; `Send`-friendly so it can
/// run on a worker thread.
pub fn render_page(pdf: &Pdf, index: usize, max_px: u32) -> Option<PageRaster> {
    pdf.pages().get(index).map(|page| PageRaster::render(page, max_px))
}

#[allow(dead_code)]
fn _assert_send_sync() {
    fn f<T: Send + Sync>() {}
    f::<Pdf>();
    f::<PageRaster>();
}
