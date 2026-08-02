//! WebAssembly bindings for zpdf.
//!
//! Exposes the read pipeline — open, page geometry, CPU raster rendering,
//! text extraction, SVG export — to JavaScript via `wasm-bindgen`. Pure Rust
//! all the way down (no C dependencies), which is what makes this build
//! possible on `wasm32-unknown-unknown` in the first place.
//!
//! Wall-clock anti-hang budgets are disabled on this target (bare wasm32 has
//! no OS clock; see `zpdf_core::time`); the deterministic budgets in
//! `ParseLimits` (operator/command counts, pixel caps) still bound
//! adversarial inputs.

use wasm_bindgen::prelude::*;
use zpdf::RenderBackend;

/// One-time module init: surface panics as readable console errors.
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

fn js_err(e: impl std::fmt::Display) -> JsError {
    JsError::new(&e.to_string())
}

/// A rendered page raster: tightly packed RGBA8, ready for
/// `ImageData`/`putImageData` (the page composites over an opaque background,
/// so premultiplied and straight alpha coincide).
#[wasm_bindgen]
pub struct PageBitmap {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

#[wasm_bindgen]
impl PageBitmap {
    #[wasm_bindgen(getter)]
    pub fn width(&self) -> u32 {
        self.width
    }

    #[wasm_bindgen(getter)]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The RGBA8 pixels (copied out to a `Uint8Array`).
    pub fn rgba(&self) -> Vec<u8> {
        self.rgba.clone()
    }
}

/// An open PDF document.
#[wasm_bindgen]
pub struct Pdf {
    doc: zpdf::PdfDocument,
    icc: zpdf::IccCache,
}

#[wasm_bindgen]
impl Pdf {
    /// Open a PDF from bytes (empty-password encrypted files open too).
    pub fn open(data: &[u8]) -> Result<Pdf, JsError> {
        let doc = zpdf::PdfDocument::open(data.to_vec()).map_err(js_err)?;
        Ok(Pdf {
            doc,
            icc: zpdf::IccCache::new(),
        })
    }

    /// Open an encrypted PDF with a user or owner password.
    pub fn open_with_password(data: &[u8], password: &str) -> Result<Pdf, JsError> {
        let doc = zpdf::PdfDocument::open_with_password(data.to_vec(), password.as_bytes())
            .map_err(js_err)?;
        Ok(Pdf {
            doc,
            icc: zpdf::IccCache::new(),
        })
    }

    #[wasm_bindgen(getter)]
    pub fn page_count(&self) -> usize {
        self.doc.page_count()
    }

    /// Document title from the info dictionary, if any.
    #[wasm_bindgen(getter)]
    pub fn title(&self) -> Option<String> {
        self.doc.info().and_then(|i| i.title)
    }

    /// `[width, height]` of a page's effective box in PDF points (72 dpi),
    /// with `/Rotate` taken into account (90°/270° swap the sides).
    pub fn page_size(&self, index: usize) -> Result<Vec<f64>, JsError> {
        let page = self.doc.page(index).map_err(js_err)?;
        let rect = page.effective_box();
        let (w, h) = if page.rotate % 180 != 0 {
            (rect.height(), rect.width())
        } else {
            (rect.width(), rect.height())
        };
        Ok(vec![w, h])
    }

    /// Rasterize a page with the CPU backend. `scale` is relative to 72 dpi
    /// (i.e. `scale = dpi / 72`), clamped to a sane range.
    pub fn render_page(&mut self, index: usize, scale: f32) -> Result<PageBitmap, JsError> {
        let scale = if scale.is_finite() {
            scale.clamp(0.05, 8.0)
        } else {
            1.0
        };
        let page = self.doc.page(index).map_err(js_err)?;
        let mut fonts = self.doc.load_page_fonts(&page);
        let mut images = zpdf::ImageCache::new();
        let content = self.doc.page_content_bytes(&page).map_err(js_err)?;
        let dl = zpdf::ContentInterpreter::new(page.effective_box())
            .with_page_rotation(page.rotate)
            .with_fonts(&mut fonts)
            .with_document(self.doc.file(), &page.resources)
            .with_images(&mut images)
            .with_colors(&mut self.icc)
            .with_operand_stack_limit(self.doc.file().limits().max_operand_stack_depth as usize)
            .interpret(&content);

        let target = zpdf::cpu::CpuRenderer::new()
            .with_limits(self.doc.file().limits())
            .with_fonts(&fonts)
            .with_images(&images)
            .render_display_list(&dl, scale)
            .map_err(js_err)?;
        Ok(PageBitmap {
            width: target.width,
            height: target.height,
            rgba: target.data,
        })
    }

    /// Extract a page's text in reading order (struct-tree order when the
    /// document is tagged, geometric order otherwise).
    pub fn page_text(&mut self, index: usize) -> Result<String, JsError> {
        let page = self.doc.page(index).map_err(js_err)?;
        let mut fonts = self.doc.load_page_fonts(&page);
        let content = self.doc.page_content_bytes(&page).map_err(js_err)?;
        let mut spans: Vec<zpdf::TextSpan> = Vec::new();
        {
            let interpreter = zpdf::ContentInterpreter::new(page.effective_box())
                .with_fonts(&mut fonts)
                .with_document(self.doc.file(), &page.resources)
                .with_colors(&mut self.icc)
                .with_text_sink(&mut spans)
                .with_operand_stack_limit(
                    self.doc.file().limits().max_operand_stack_depth as usize,
                );
            let _ = interpreter.interpret(&content);
        }
        Ok(match self.doc.struct_tree() {
            Some(tree) => zpdf::struct_ordered_text(&spans, index, &tree),
            None => zpdf::spans_to_text(spans, 2.0),
        })
    }

    /// Export a page as a standalone vector SVG document.
    pub fn page_svg(&mut self, index: usize) -> Result<String, JsError> {
        let page = self.doc.page(index).map_err(js_err)?;
        let mut fonts = self.doc.load_page_fonts(&page);
        let mut images = zpdf::ImageCache::new();
        let content = self.doc.page_content_bytes(&page).map_err(js_err)?;
        let dl = zpdf::ContentInterpreter::new(page.effective_box())
            .with_page_rotation(page.rotate)
            .with_fonts(&mut fonts)
            .with_document(self.doc.file(), &page.resources)
            .with_images(&mut images)
            .with_colors(&mut self.icc)
            .with_operand_stack_limit(self.doc.file().limits().max_operand_stack_depth as usize)
            .interpret(&content);
        Ok(zpdf_svg_export::display_list_to_svg(
            &dl,
            &fonts,
            &images,
            &zpdf_svg_export::SvgOptions::default(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal single-page PDF exercising open → size → render → text → svg
    /// natively (the same code paths the wasm build runs).
    fn tiny_pdf() -> Vec<u8> {
        let content = b"0 0 1 rg 20 20 80 80 re f\nBT /F1 12 Tf 30 160 Td (Hi) Tj ET";
        let font = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>";
        let objs: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(),
            {
                let mut v = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
                v.extend_from_slice(content);
                v.extend_from_slice(b"\nendstream");
                v
            },
            font.to_vec(),
        ];
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref = out.len();
        out.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes(),
        );
        for off in &offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objs.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    #[test]
    fn full_read_pipeline_works() {
        let mut pdf = Pdf::open(&tiny_pdf()).expect("open");
        assert_eq!(pdf.page_count(), 1);
        assert_eq!(pdf.page_size(0).unwrap(), vec![200.0, 200.0]);

        let bitmap = pdf.render_page(0, 1.0).expect("render");
        assert_eq!((bitmap.width, bitmap.height), (200, 200));
        assert_eq!(bitmap.rgba.len(), 200 * 200 * 4);
        // The blue rect must have painted: some pixel is dominated by blue.
        assert!(bitmap
            .rgba
            .chunks_exact(4)
            .any(|px| px[2] > 200 && px[0] < 60));

        let text = pdf.page_text(0).expect("text");
        assert!(text.contains("Hi"), "{text}");

        let svg = pdf.page_svg(0).expect("svg");
        assert!(svg.starts_with("<?xml"), "{svg}");
        assert!(svg.contains("fill=\"#0000ff\""), "{svg}");
    }
}
