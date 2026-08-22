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

// ---------------------------------------------------------------------------
// Editing surface
// ---------------------------------------------------------------------------

/// A mutable PDF editor wrapping an [`zpdf_writer::IncrementalWriter`]. Open a
/// document, queue any number of edits (page ops, metadata, stamps,
/// annotations, redaction, form fill, merge), then call [`PdfEditor::save`] to
/// serialize them as **one incremental update** (the original bytes plus the
/// new revision — the smallest possible output, matching the CLI's
/// composition). Page indices are 0-based (JS convention; the CLI is 1-based).
///
/// `split_pages` and `optimize` are one-shot free functions instead — they
/// fully rewrite or extract, so they don't fit the incremental-editor model.
#[wasm_bindgen]
pub struct PdfEditor {
    writer: zpdf_writer::IncrementalWriter,
}

/// Host-testable core: serialize a writer's pending edits to a byte vec.
/// (`IncrementalWriter::write` needs `Write + Seek`; `Cursor<Vec<u8>>` provides
/// both on wasm32 and on the host.)
pub(crate) fn editor_save_bytes(
    writer: &zpdf_writer::IncrementalWriter,
) -> std::io::Result<Vec<u8>> {
    let mut cur = std::io::Cursor::new(Vec::new());
    writer.write(&mut cur)?;
    Ok(cur.into_inner())
}

/// Host-testable core: chunk a flat `[x0,y0,x1,y1, …]` into `Rect`s. Returns a
/// `String` error (not `JsError`, which can't be constructed on a non-wasm
/// host) so the length check is unit-testable without a browser.
//
// `is_multiple_of` is Rust 1.87+; the workspace pins no MSRV and CI rides
// `stable`, so use the universally-available `% 4` form and silence the
// clippy `manual_is_multiple_of` suggestion it would otherwise make.
#[allow(clippy::manual_is_multiple_of)]
pub(crate) fn rects_from_flat(flat: &[f64]) -> Result<Vec<zpdf::Rect>, String> {
    if flat.len() % 4 != 0 {
        return Err(
            "expected a flat [x0,y0,x1,y1, ...] array whose length is a multiple of 4".into(),
        );
    }
    Ok(flat
        .chunks_exact(4)
        .map(|c| zpdf::Rect::new(c[0], c[1], c[2], c[3]))
        .collect())
}

/// Host-testable core: run a `FormFiller` session (new → set each → finish).
/// `String` error so the no-AcroForm path is unit-testable on a non-wasm host.
pub(crate) fn fill_fields_core(
    writer: &mut zpdf_writer::IncrementalWriter,
    names: Vec<String>,
    values: Vec<String>,
) -> Result<(), String> {
    if names.len() != values.len() {
        return Err("fill_fields: names and values must be the same length".into());
    }
    let mut filler = zpdf_writer::FormFiller::new(writer).map_err(|e| e.to_string())?;
    for (n, v) in names.into_iter().zip(values) {
        filler.set(&n, &v).map_err(|e| e.to_string())?;
    }
    filler.finish().map_err(|e| e.to_string())
}

/// Build a 3-component colour from three optional channels: `Some` only when
/// all three are present (so a JS caller can omit colour entirely by passing
/// `null`s).
pub(crate) fn color3(r: Option<f64>, g: Option<f64>, b: Option<f64>) -> Option<(f64, f64, f64)> {
    match (r, g, b) {
        (Some(r), Some(g), Some(b)) => Some((r, g, b)),
        _ => None,
    }
}

#[wasm_bindgen]
#[allow(clippy::too_many_arguments)] // FFI surface: each edit takes its operands flat
impl PdfEditor {
    /// Open a PDF for editing (empty-password encrypted files open too).
    pub fn open(data: &[u8]) -> Result<PdfEditor, JsError> {
        let writer = zpdf_writer::IncrementalWriter::new(data.to_vec()).map_err(js_err)?;
        Ok(PdfEditor { writer })
    }

    /// Open an encrypted PDF with a user or owner password.
    pub fn open_with_password(data: &[u8], password: &str) -> Result<PdfEditor, JsError> {
        let writer =
            zpdf_writer::IncrementalWriter::new_with_password(data.to_vec(), password.as_bytes())
                .map_err(js_err)?;
        Ok(PdfEditor { writer })
    }

    #[wasm_bindgen(getter)]
    pub fn page_count(&self) -> usize {
        self.writer.document().page_count()
    }

    /// Serialize all queued edits as one incremental update → `Uint8Array`.
    pub fn save(&self) -> Result<Vec<u8>, JsError> {
        editor_save_bytes(&self.writer).map_err(js_err)
    }

    // — page operations ————————————————————————————————————————————————

    /// Rotate a page by a multiple of 90° (cumulative on existing /Rotate).
    pub fn rotate_page(&mut self, page: usize, degrees: i32) -> Result<(), JsError> {
        self.writer.rotate_page(page, degrees).map_err(js_err)
    }

    /// Delete the given (0-based) pages. Deleting every page is an error.
    pub fn delete_pages(&mut self, pages: Vec<usize>) -> Result<(), JsError> {
        self.writer.delete_pages(&pages).map_err(js_err)
    }

    /// Reorder pages: `order[i]` is the original index of the page that becomes
    /// new page `i` (a full permutation of `0..page_count`).
    pub fn reorder_pages(&mut self, order: Vec<usize>) -> Result<(), JsError> {
        self.writer.reorder_pages(&order).map_err(js_err)
    }

    // — metadata ——————————————————————————————————————————————————————
    // Each setter builds a one-field `InfoUpdate`: not calling the setter
    // leaves the field unchanged; `Some(s)` sets it; `None` deletes it.

    pub fn set_title(&mut self, value: Option<String>) -> Result<(), JsError> {
        let u = zpdf_writer::InfoUpdate {
            title: Some(value),
            ..Default::default()
        };
        self.writer.set_info(&u).map_err(js_err)
    }
    pub fn set_author(&mut self, value: Option<String>) -> Result<(), JsError> {
        let u = zpdf_writer::InfoUpdate {
            author: Some(value),
            ..Default::default()
        };
        self.writer.set_info(&u).map_err(js_err)
    }
    pub fn set_subject(&mut self, value: Option<String>) -> Result<(), JsError> {
        let u = zpdf_writer::InfoUpdate {
            subject: Some(value),
            ..Default::default()
        };
        self.writer.set_info(&u).map_err(js_err)
    }
    pub fn set_keywords(&mut self, value: Option<String>) -> Result<(), JsError> {
        let u = zpdf_writer::InfoUpdate {
            keywords: Some(value),
            ..Default::default()
        };
        self.writer.set_info(&u).map_err(js_err)
    }
    pub fn set_creator(&mut self, value: Option<String>) -> Result<(), JsError> {
        let u = zpdf_writer::InfoUpdate {
            creator: Some(value),
            ..Default::default()
        };
        self.writer.set_info(&u).map_err(js_err)
    }
    pub fn set_producer(&mut self, value: Option<String>) -> Result<(), JsError> {
        let u = zpdf_writer::InfoUpdate {
            producer: Some(value),
            ..Default::default()
        };
        self.writer.set_info(&u).map_err(js_err)
    }

    // — stamping ——————————————————————————————————————————————————————————

    /// Stamp a text string on a page in a Standard-14 font. `r/g/b` are
    /// DeviceRGB in `[0,1]`; `font` is a Standard-14 name (e.g. `"Helvetica"`).
    pub fn stamp_text(
        &mut self,
        page: usize,
        text: &str,
        x: f64,
        y: f64,
        font: &str,
        size: f64,
        r: f64,
        g: f64,
        b: f64,
    ) -> Result<(), JsError> {
        let item = zpdf_writer::StampItem::Text {
            text: text.to_string(),
            x,
            y,
            font: font.to_string(),
            size,
            color: (r, g, b),
        };
        self.writer.stamp_page(page, &[item]).map_err(js_err)
    }

    /// Stamp a raw RGBA8 image on a page. `pixels` is tightly packed
    /// `width*height*4` bytes; the alpha channel becomes a `/SMask`.
    pub fn stamp_image_rgba(
        &mut self,
        page: usize,
        pixels: &[u8],
        width: u32,
        height: u32,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    ) -> Result<(), JsError> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or_else(|| JsError::new("stamp_image_rgba: width*height*4 overflows"))?;
        if pixels.len() < expected {
            return Err(JsError::new(
                "stamp_image_rgba: pixel buffer smaller than width*height*4",
            ));
        }
        let image = zpdf_writer::StampImage::Rgba8 {
            width,
            height,
            pixels: pixels.to_vec(),
        };
        let item = zpdf_writer::StampItem::Image {
            image,
            x,
            y,
            width: w,
            height: h,
        };
        self.writer.stamp_page(page, &[item]).map_err(js_err)
    }

    // — annotations (one method per kind — typed FFI beats string dispatch) —

    pub fn add_highlight(
        &mut self,
        page: usize,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        r: f64,
        g: f64,
        b: f64,
    ) -> Result<(), JsError> {
        self.add_markup(
            zpdf_writer::MarkupKind::Highlight,
            page,
            x0,
            y0,
            x1,
            y1,
            r,
            g,
            b,
        )
    }
    pub fn add_underline(
        &mut self,
        page: usize,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        r: f64,
        g: f64,
        b: f64,
    ) -> Result<(), JsError> {
        self.add_markup(
            zpdf_writer::MarkupKind::Underline,
            page,
            x0,
            y0,
            x1,
            y1,
            r,
            g,
            b,
        )
    }
    pub fn add_strikeout(
        &mut self,
        page: usize,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        r: f64,
        g: f64,
        b: f64,
    ) -> Result<(), JsError> {
        self.add_markup(
            zpdf_writer::MarkupKind::StrikeOut,
            page,
            x0,
            y0,
            x1,
            y1,
            r,
            g,
            b,
        )
    }
    pub fn add_squiggly(
        &mut self,
        page: usize,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        r: f64,
        g: f64,
        b: f64,
    ) -> Result<(), JsError> {
        self.add_markup(
            zpdf_writer::MarkupKind::Squiggly,
            page,
            x0,
            y0,
            x1,
            y1,
            r,
            g,
            b,
        )
    }
    fn add_markup(
        &mut self,
        kind: zpdf_writer::MarkupKind,
        page: usize,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        r: f64,
        g: f64,
        b: f64,
    ) -> Result<(), JsError> {
        let rect = zpdf::Rect::new(x0, y0, x1, y1);
        let spec = zpdf_writer::AnnotationSpec::markup_from_rects(kind, &[rect], (r, g, b), None);
        self.writer.add_annotation(page, &spec).map_err(js_err)?;
        Ok(())
    }

    pub fn add_note(
        &mut self,
        page: usize,
        x: f64,
        y: f64,
        contents: &str,
        r: Option<f64>,
        g: Option<f64>,
        b: Option<f64>,
        icon: Option<String>,
    ) -> Result<(), JsError> {
        let spec = zpdf_writer::AnnotationSpec::Note {
            x,
            y,
            contents: contents.to_string(),
            color: color3(r, g, b),
            icon,
        };
        self.writer.add_annotation(page, &spec).map_err(js_err)?;
        Ok(())
    }

    pub fn add_freetext(
        &mut self,
        page: usize,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        contents: &str,
        size: Option<f64>,
        r: Option<f64>,
        g: Option<f64>,
        b: Option<f64>,
    ) -> Result<(), JsError> {
        let spec = zpdf_writer::AnnotationSpec::FreeText {
            rect: zpdf::Rect::new(x0, y0, x1, y1),
            contents: contents.to_string(),
            size,
            color: color3(r, g, b),
        };
        self.writer.add_annotation(page, &spec).map_err(js_err)?;
        Ok(())
    }

    pub fn add_square(
        &mut self,
        page: usize,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        r: f64,
        g: f64,
        b: f64,
        ir: Option<f64>,
        ig: Option<f64>,
        ib: Option<f64>,
        width: f64,
    ) -> Result<(), JsError> {
        let spec = zpdf_writer::AnnotationSpec::Square {
            rect: zpdf::Rect::new(x0, y0, x1, y1),
            color: (r, g, b),
            interior: color3(ir, ig, ib),
            width,
        };
        self.writer.add_annotation(page, &spec).map_err(js_err)?;
        Ok(())
    }

    pub fn add_circle(
        &mut self,
        page: usize,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        r: f64,
        g: f64,
        b: f64,
        ir: Option<f64>,
        ig: Option<f64>,
        ib: Option<f64>,
        width: f64,
    ) -> Result<(), JsError> {
        let spec = zpdf_writer::AnnotationSpec::Circle {
            rect: zpdf::Rect::new(x0, y0, x1, y1),
            color: (r, g, b),
            interior: color3(ir, ig, ib),
            width,
        };
        self.writer.add_annotation(page, &spec).map_err(js_err)?;
        Ok(())
    }

    pub fn add_line(
        &mut self,
        page: usize,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        r: f64,
        g: f64,
        b: f64,
        width: f64,
    ) -> Result<(), JsError> {
        let spec = zpdf_writer::AnnotationSpec::Line {
            x1,
            y1,
            x2,
            y2,
            color: (r, g, b),
            width,
        };
        self.writer.add_annotation(page, &spec).map_err(js_err)?;
        Ok(())
    }

    // — redaction (true content removal, not just a black box) ———————————

    /// Redact regions of a page: `rects` is a flat `[x0,y0,x1,y1, …]` array;
    /// `fill` is an optional `[r,g,b]` cover colour (pass `null` to remove
    /// content without painting a box).
    pub fn redact(
        &mut self,
        page: usize,
        rects: Vec<f64>,
        fill: Option<Vec<f64>>,
    ) -> Result<(), JsError> {
        let rects = rects_from_flat(&rects).map_err(|e| JsError::new(&e))?;
        let fill = match fill {
            Some(c) => {
                if c.len() != 3 {
                    return Err(JsError::new("redact: fill must be [r, g, b]"));
                }
                Some((c[0], c[1], c[2]))
            }
            None => None,
        };
        self.writer
            .redact_page(page, &rects, &zpdf_writer::RedactOptions { fill })
            .map_err(js_err)
    }

    // — form fill (runs the whole FormFiller session in one call) —————————

    /// Fill AcroForm fields by fully-qualified name. `names` and `values` are
    /// parallel arrays (wasm-bindgen has no `Vec<(String,String)>` without
    /// serde). Errors if the document has no AcroForm, or on a signature field.
    pub fn fill_fields(&mut self, names: Vec<String>, values: Vec<String>) -> Result<(), JsError> {
        fill_fields_core(&mut self.writer, names, values).map_err(|e| JsError::new(&e))
    }

    // — merge (append another PDF's pages + outlines/AcroForm/OCGs) ——————

    /// Append all pages of `other` to this document. Returns the number of
    /// pages appended. Call `save()` to materialize.
    pub fn merge(&mut self, other: &[u8]) -> Result<usize, JsError> {
        let source = zpdf::PdfFile::parse(other.to_vec()).map_err(js_err)?;
        self.writer.append_document(&source).map_err(js_err)
    }
}

// ---------------------------------------------------------------------------
// One-shot helpers (stateless, byte-in / byte-out)
// ---------------------------------------------------------------------------

/// Extract selected (0-based) pages into a fresh self-contained PDF.
#[wasm_bindgen]
pub fn split_pages(data: &[u8], pages: Vec<usize>) -> Result<Vec<u8>, JsError> {
    let file = zpdf::PdfFile::parse(data.to_vec()).map_err(js_err)?;
    zpdf_writer::extract_pages(&file, &pages).map_err(js_err)
}

/// Options for [`optimize`]. Construct with `new OptimizeOptions()`, then set
/// fields. `linearize` and the rewrite path are mutually exclusive — setting
/// `linearize = true` ignores `compress`/`max_image_dim`.
#[wasm_bindgen]
pub struct OptimizeOptions {
    pub compress: bool,
    pub max_image_dim: Option<u32>,
    pub linearize: bool,
}

#[wasm_bindgen]
impl OptimizeOptions {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self {
            compress: true,
            max_image_dim: None,
            linearize: false,
        }
    }
}

impl Default for OptimizeOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Garbage-collect / recompress / downsample a PDF, or linearize it for fast
/// web view. (Encryption and PDF/A conversion are not exposed to wasm in this
/// round — they need more option types; use the native facade for those.)
#[wasm_bindgen]
pub fn optimize(data: &[u8], opts: OptimizeOptions) -> Result<Vec<u8>, JsError> {
    let file = zpdf::PdfFile::parse(data.to_vec()).map_err(js_err)?;
    if opts.linearize {
        zpdf_writer::linearize_pdf(&file).map_err(js_err)
    } else {
        let options = zpdf_writer::RewriteOptions {
            compress_uncompressed: opts.compress,
            max_image_dimension: opts.max_image_dim,
            encrypt: None,
            pdfa: None,
        };
        zpdf_writer::rewrite_pdf(&file, &options).map_err(js_err)
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

    // ---- PdfEditor + one-shot helpers -----------------------------------

    fn reopen(bytes: Vec<u8>) -> zpdf::PdfDocument {
        zpdf::PdfDocument::open(bytes).expect("reopen edited pdf")
    }

    #[test]
    fn editor_save_roundtrips_unchanged() {
        let ed = PdfEditor::open(&tiny_pdf()).expect("open");
        assert_eq!(ed.page_count(), 1);
        let out = ed.save().expect("save");
        let doc = reopen(out);
        assert_eq!(doc.page_count(), 1);
    }

    #[test]
    fn editor_rotate_page_persists() {
        let mut ed = PdfEditor::open(&tiny_pdf()).expect("open");
        ed.rotate_page(0, 90).expect("rotate");
        let doc = reopen(ed.save().expect("save"));
        assert_eq!(doc.page(0).unwrap().rotate, 90);
    }

    #[test]
    fn editor_set_title_persists() {
        let mut ed = PdfEditor::open(&tiny_pdf()).expect("open");
        ed.set_title(Some("Edited".into())).expect("set title");
        let doc = reopen(ed.save().expect("save"));
        assert_eq!(doc.info().and_then(|i| i.title).as_deref(), Some("Edited"));
        // Delete the title.
        let mut ed2 = PdfEditor::open(&tiny_pdf()).expect("open");
        ed2.set_title(None).expect("clear title");
        let doc2 = reopen(ed2.save().expect("save"));
        assert!(doc2.info().and_then(|i| i.title).is_none());
    }

    #[test]
    fn editor_stamp_text_succeeds() {
        let mut ed = PdfEditor::open(&tiny_pdf()).expect("open");
        ed.stamp_text(0, "DRAFT", 10.0, 10.0, "Helvetica", 24.0, 1.0, 0.0, 0.0)
            .expect("stamp");
        let doc = reopen(ed.save().expect("save"));
        assert_eq!(doc.page_count(), 1);
    }

    #[test]
    fn editor_add_highlight_adds_annotation() {
        let mut ed = PdfEditor::open(&tiny_pdf()).expect("open");
        ed.add_highlight(0, 10.0, 10.0, 90.0, 50.0, 1.0, 1.0, 0.0)
            .expect("highlight");
        let doc = reopen(ed.save().expect("save"));
        let page = doc.page(0).unwrap();
        assert_eq!(
            doc.page_annotations(&page).len(),
            1,
            "exactly one annotation after add_highlight"
        );
    }

    #[test]
    fn editor_redact_succeeds() {
        let mut ed = PdfEditor::open(&tiny_pdf()).expect("open");
        ed.redact(0, vec![10.0, 10.0, 50.0, 50.0], None)
            .expect("redact");
        let doc = reopen(ed.save().expect("save"));
        assert_eq!(doc.page_count(), 1);
    }

    #[test]
    fn editor_fill_fields_errors_without_acroform() {
        let mut ed = PdfEditor::open(&tiny_pdf()).expect("open");
        // Call the host-testable core directly: the wasm `fill_fields` wrapper
        // maps the error through `JsError::new`, which can't run on a non-wasm
        // host. The core returns a `String` error.
        assert!(
            fill_fields_core(&mut ed.writer, vec!["x".into()], vec!["y".into()]).is_err(),
            "fill_fields on a form-less PDF must error"
        );
    }

    #[test]
    fn editor_merge_appends_pages() {
        let mut ed = PdfEditor::open(&tiny_pdf()).expect("open");
        assert_eq!(ed.merge(&tiny_pdf()).expect("merge"), 1);
        let doc = reopen(ed.save().expect("save"));
        assert_eq!(doc.page_count(), 2);
    }

    #[test]
    fn split_pages_extracts_one() {
        let out = split_pages(&tiny_pdf(), vec![0]).expect("split");
        assert_eq!(reopen(out).page_count(), 1);
    }

    #[test]
    fn optimize_rewrite_and_linearize_keep_pages() {
        let rewritten = optimize(
            &tiny_pdf(),
            OptimizeOptions {
                compress: true,
                max_image_dim: None,
                linearize: false,
            },
        )
        .expect("optimize rewrite");
        assert_eq!(reopen(rewritten).page_count(), 1);

        let linearized = optimize(
            &tiny_pdf(),
            OptimizeOptions {
                compress: true,
                max_image_dim: None,
                linearize: true,
            },
        )
        .expect("optimize linearize");
        assert_eq!(reopen(linearized).page_count(), 1);
    }

    #[test]
    fn rects_from_flat_validates_length() {
        assert_eq!(
            rects_from_flat(&[0.0, 0.0, 10.0, 10.0, 1.0, 2.0, 3.0, 4.0])
                .expect("two rects")
                .len(),
            2
        );
        assert!(
            rects_from_flat(&[1.0, 2.0, 3.0]).is_err(),
            "length not a multiple of 4"
        );
        assert!(color3(Some(1.0), Some(0.0), Some(0.0)).is_some());
        assert!(color3(Some(1.0), None, Some(0.0)).is_none());
    }
}
