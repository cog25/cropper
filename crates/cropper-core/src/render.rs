//! Rasterize PDF pages with hayro and convert them to grayscale.

use hayro::hayro_syntax::page::Page;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::hayro_interpret::InterpreterSettings;
use hayro::{RenderCache, RenderSettings};

/// Long-side cap on rendered pages; also keeps width/height below the u16
/// limit hayro's viewport expects.
const MAX_PX_LIMIT: u32 = 4000;

/// A page rendered to grayscale, plus the mapping from pixels to points.
pub struct PageRaster {
    pub width: u32,
    pub height: u32,
    /// Rendered page size in points (display orientation).
    pub page_width: f32,
    pub page_height: f32,
    /// Luma samples, row-major, `width * height` bytes.
    pub luma: Vec<u8>,
}

impl PageRaster {
    /// Render `page` so that its long side is roughly `max_px` pixels.
    pub fn render(page: &Page, max_px: u32) -> PageRaster {
        let max_px = max_px.clamp(64, MAX_PX_LIMIT);
        let (page_width, page_height) = page.render_dimensions();
        let scale = max_px as f32 / page_width.max(page_height);
        let width = ((page_width * scale).floor() as u32).max(1);
        let height = ((page_height * scale).floor() as u32).max(1);

        let cache = RenderCache::new();
        let interp = InterpreterSettings::default();
        let settings = RenderSettings {
            x_scale: width as f32 / page_width,
            y_scale: height as f32 / page_height,
            width: Some(width as u16),
            height: Some(height as u16),
            bg_color: WHITE,
        };
        let pixmap = hayro::render(page, &cache, &interp, &settings);
        let rgba = pixmap.data_as_u8_slice();

        let mut luma = vec![0u8; (width * height) as usize];
        for (i, out) in luma.iter_mut().enumerate() {
            let px = &rgba[i * 4..i * 4 + 3];
            // Premultiplied RGBA over a white background: alpha is 255, so the
            // channels can be used directly.
            *out = ((px[0] as u32 * 299 + px[1] as u32 * 587 + px[2] as u32 * 114) / 1000) as u8;
        }

        PageRaster { width, height, page_width, page_height, luma }
    }

    /// Grayscale PNG encoding of the raster (for previews).
    pub fn to_png(&self) -> Result<Vec<u8>, String> {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut encoder = png::Encoder::new(&mut buf, self.width, self.height);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(&self.luma).map_err(|e| e.to_string())?;
        writer.finish().map_err(|e| e.to_string())?;
        Ok(buf.into_inner())
    }
}
