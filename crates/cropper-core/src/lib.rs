//! Cropper — margin cropping of scanned PDFs.
//!
//! The pipeline is:
//! 1. hayro rasterizes each page (rotation applied, white background),
//! 2. a luma-threshold detector finds the ink bounding box,
//! 3. krilla rewrites the document with cropped page boxes while embedding
//!    the original pages verbatim.
//!
//! Shared pipeline state lives in [`session`], final crop math in [`plan`].

pub mod detect;
pub mod export;
pub mod plan;
pub mod render;
pub mod session;
pub mod types;

#[cfg(target_arch = "wasm32")]
mod wasm {
    use serde::Deserialize;
    use wasm_bindgen::prelude::*;

    use crate::session::Session;
    use crate::types::{CropRect, DetectOptions, PageAnalysis, PlacedCrop};

    /// Matches the JS-side crop JSON: `[{"x":..,"y":..,"width":..,"height":..}, null, ..]`.
    #[derive(Deserialize)]
    struct JsCrop {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
    }

    #[wasm_bindgen]
    pub struct Engine {
        session: Session,
    }

    #[wasm_bindgen]
    impl Engine {
        #[wasm_bindgen(constructor)]
        pub fn new(bytes: &[u8]) -> Result<Engine, JsError> {
            console_error_panic_hook::set_once();
            let session = Session::new(bytes.to_vec()).map_err(|e| JsError::new(&e))?;
            Ok(Engine { session })
        }

        pub fn page_count(&self) -> usize {
            self.session.page_count()
        }

        /// Page size in points, display orientation: `[width, height]`.
        pub fn page_size(&self, index: usize) -> Result<Vec<f32>, JsError> {
            let (width, height) = self.session.page_size(index).map_err(|e| JsError::new(&e))?;
            Ok(vec![width, height])
        }

        /// Render + detect one page. Caches the raster so later calls with
        /// the same `max_px` skip rendering entirely.
        ///
        /// Returns a JSON `PageAnalysis`.
        pub fn analyze_page(
            &mut self,
            index: usize,
            max_px: u32,
            threshold: u8,
            min_ink: u32,
        ) -> Result<String, JsError> {
            let (page_width, page_height) =
                self.session.page_size(index).map_err(|e| JsError::new(&e))?;
            let options = DetectOptions { threshold, min_ink, ..Default::default() };
            let (crop, ink_ratio, blank, _t) = self
                .session
                .analyze(index, max_px, &options)
                .map_err(|e| JsError::new(&e))?;
            let analysis =
                PageAnalysis { page_width, page_height, crop, ink_ratio, blank };
            serde_json::to_string(&analysis)
                .map_err(|e| JsError::new(&format!("분석 결과 직렬화 실패: {e}")))
        }

        /// Grayscale PNG of the full page as last analyzed (for previews).
        pub fn thumbnail_png(&self, index: usize) -> Result<Option<Vec<u8>>, JsError> {
            match self.session.raster(index) {
                Some(raster) => raster.to_png().map(Some).map_err(|e| JsError::new(&e)),
                None => Ok(None),
            }
        }

        /// Build the cropped PDF.
        ///
        /// `crops_json` is an array with one entry per page: a
        /// `{x,y,width,height}` object or `null` to keep the page intact.
        pub fn export(&self, crops_json: &str) -> Result<Vec<u8>, JsError> {
            let crops: Vec<Option<JsCrop>> = serde_json::from_str(crops_json)
                .map_err(|e| JsError::new(&format!("자르기 정보 해석 실패: {e}")))?;
            if crops.len() != self.session.page_count() {
                return Err(JsError::new("자르기 정보가 페이지 수와 일치하지 않습니다."));
            }
            let crops: Vec<Option<PlacedCrop>> = crops
                .into_iter()
                .map(|c| c.map(|c| PlacedCrop::on_page(CropRect::new(c.x, c.y, c.width, c.height))))
                .collect();
            self.session.export(&crops).map_err(|e| JsError::new(&e))
        }
    }
}
