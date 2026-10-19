//! Produce cropped PDFs with krilla: each page gets a new MediaBox and the
//! original page is embedded as a Form XObject drawn through a transform that
//! maps the source crop onto it. The original image streams are copied
//! verbatim, so export is fast and lossless; scaling is pure transform.

use std::sync::Arc;

use krilla::configure::{ConfigurationBuilder, PdfVersion};
use krilla::geom::{PathBuilder, Rect, Size, Transform};
use krilla::page::PageSettings;
use krilla::pdf::{Pdf, PdfDocument};
use krilla::paint::FillRule;
use krilla::{Document, SerializeSettings};
use krilla::metadata::Metadata;

use crate::types::{CropRect, PlacedCrop};

/// Crop each page of `source`. `placed` is one entry per page; `None` keeps
/// the page untouched at its original size. Coordinates are points in display
/// space (top-left, y-down), i.e. exactly what hayro renders and what our
/// detector emits.
pub fn export_cropped(source: &[u8], placed: &[Option<PlacedCrop>]) -> Result<Vec<u8>, String> {
    let pdf = Pdf::new(source.to_vec()).map_err(|_| "PDF 파일을 열 수 없습니다".to_string())?;

    // The krilla output must be at least as new as the embedded document,
    // otherwise krilla refuses to embed it.
    let out_version = match pdf.version() {
        hayro::hayro_syntax::PdfVersion::Pdf20 => PdfVersion::Pdf20,
        _ => PdfVersion::Pdf17,
    };
    let configuration = ConfigurationBuilder::new()
        .with_version(out_version)
        .finish()
        .map_err(|e| format!("{e:?}"))?;

    // Display-space page sizes, read before the document is handed over.
    let dims: Vec<(f32, f32)> = pdf.pages().iter().map(|p| p.render_dimensions()).collect();
    if dims.is_empty() {
        return Err("문서에 페이지가 없습니다".to_string());
    }
    if placed.len() != dims.len() {
        return Err("자르기 정보가 페이지 수와 일치하지 않습니다".to_string());
    }

    let settings = SerializeSettings {
        configuration,
        xmp_metadata: false,
        enable_tagging: false,
        ..SerializeSettings::default()
    };
    let mut doc = Document::new_with(settings);
    doc.set_metadata(
        Metadata::new()
            .creator("Cropper".to_string())
            .producer("Cropper (krilla)".to_string()),
    );

    let embedded = PdfDocument::new(Arc::new(pdf));

    for (index, &(page_width, page_height)) in dims.iter().enumerate() {
        let entry = placed[index];
        let crop = entry
            .map(|p| p.crop.clamped(page_width, page_height))
            .unwrap_or_else(|| CropRect::full(page_width, page_height));
        let (out_w, out_h, transform) = match entry {
            Some(p) => {
                // out = scale * (src - crop.min) + offset
                let tx = p.offset_x - p.scale * crop.x;
                let ty = p.offset_y - p.scale * crop.y;
                (
                    p.page_width.max(2.0),
                    p.page_height.max(2.0),
                    Transform::from_row(p.scale, 0.0, 0.0, p.scale, tx, ty),
                )
            }
            None => (
                page_width,
                page_height,
                Transform::from_translate(-crop.x, -crop.y),
            ),
        };

        let page_settings = PageSettings::from_wh(out_w, out_h)
            .ok_or("페이지 크기가 올바르지 않습니다")?;
        let mut page = doc.start_page_with(page_settings);
        let mut surface = page.surface();

        // Clip against the visible page so content cannot bleed outside,
        // then map the embedded page by the placement transform.
        let mut builder = PathBuilder::new();
        builder.push_rect(Rect::from_xywh(0.0, 0.0, out_w, out_h).unwrap());
        let clip = builder.finish().unwrap();
        surface.push_clip_path(&clip, &FillRule::NonZero);
        surface.push_transform(&transform);
        let size = Size::from_wh(page_width, page_height).ok_or("페이지 크기가 올바르지 않습니다")?;
        surface.draw_pdf_page(&embedded, size, index);
        // draw_pdf_page balances its own pushes; ours need explicit pops.
        surface.pop();
        surface.pop();

        surface.finish();
        page.finish();
    }

    doc.finish().map_err(|e| e.to_string())
}
