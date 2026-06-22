use std::path::PathBuf;
use std::sync::Arc;

use gpui::RenderImage;
use image::{Frame, RgbaImage};
use smallvec::SmallVec;
use thiserror::Error;
use zpdf::gpu::{GpuContext, WgpuRenderError, WgpuRenderer};
use zpdf::{ContentInterpreter, IccCache, ImageCache, PdfDocument, RenderBackend};

pub struct LoadedDocument {
    pub summary: DocumentSummary,
    pdf: PdfDocument,
}

#[derive(Debug, Clone)]
pub struct DocumentSummary {
    pub path: PathBuf,
    pub version: (u8, u8),
    pub page_count: usize,
    pub pages: Vec<PageSummary>,
}

#[derive(Debug, Clone)]
pub struct PageSummary {
    pub index: usize,
    pub width: f64,
    pub height: f64,
    pub rotate: i32,
}

#[derive(Clone)]
pub struct PagePreview {
    pub image: Arc<RenderImage>,
    pub pixel_width: f32,
    pub pixel_height: f32,
}

#[derive(Debug, Error)]
pub enum DocumentError {
    #[error("failed to read {path}: {source}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to open PDF {path}: {source}")]
    OpenPdf {
        path: PathBuf,
        #[source]
        source: zpdf::Error,
    },
    #[error("failed to read page {page} from {path}: {source}")]
    ReadPage {
        path: PathBuf,
        page: usize,
        #[source]
        source: zpdf::Error,
    },
    #[error("failed to read content stream for page {page} from {path}: {source}")]
    ReadPageContent {
        path: PathBuf,
        page: usize,
        #[source]
        source: zpdf::Error,
    },
    #[error("failed to rasterize page {page} from {path}: {source}")]
    RenderPage {
        path: PathBuf,
        page: usize,
        #[source]
        source: WgpuRenderError,
    },
    #[error("failed to build page image for page {page} from {path}")]
    BuildPageImage { path: PathBuf, page: usize },
}

pub fn load_document(path: PathBuf) -> Result<LoadedDocument, DocumentError> {
    let data = std::fs::read(&path).map_err(|source| DocumentError::ReadFile {
        path: path.clone(),
        source,
    })?;
    let pdf = PdfDocument::open(data).map_err(|source| DocumentError::OpenPdf {
        path: path.clone(),
        source,
    })?;

    let mut pages = Vec::with_capacity(pdf.page_count());
    for index in 0..pdf.page_count() {
        let page = pdf.page(index).map_err(|source| DocumentError::ReadPage {
            path: path.clone(),
            page: index + 1,
            source,
        })?;
        // Report the rendered (effective) page box so the sidebar/footer point
        // dimensions match what the viewer actually rasterizes; rotation is shown
        // separately, as page size is conventionally reported pre-rotation.
        let eb = page.effective_box();
        pages.push(PageSummary {
            index,
            width: eb.width(),
            height: eb.height(),
            rotate: page.rotate,
        });
    }

    Ok(LoadedDocument {
        summary: DocumentSummary {
            path,
            version: pdf.version(),
            page_count: pdf.page_count(),
            pages,
        },
        pdf,
    })
}

impl LoadedDocument {
    pub fn render_page_preview(
        &self,
        index: usize,
        dpi: f32,
        context_slot: &mut Option<GpuContext>,
    ) -> Result<PagePreview, DocumentError> {
        let page = self
            .pdf
            .page(index)
            .map_err(|source| DocumentError::ReadPage {
                path: self.summary.path.clone(),
                page: index + 1,
                source,
            })?;
        let mut fonts = self.pdf.load_page_fonts(&page);
        let content = self.pdf.page_content_bytes(&page).map_err(|source| {
            DocumentError::ReadPageContent {
                path: self.summary.path.clone(),
                page: index + 1,
                source,
            }
        })?;
        let mut images = ImageCache::new();
        let mut colors = IccCache::new();
        // Output intents (PDF/X & PDF 2.0): colour-manage DeviceCMYK through the
        // page's/document's CMYK /DestOutputProfile when present, matching the CLI.
        let doc_intents = self.pdf.output_intents();
        let oi_cmyk = zpdf::output_intent_cmyk_profile(
            self.pdf.file(),
            self.pdf.page_output_intents(&page),
            &doc_intents,
            &mut colors,
        );
        // Render the effective box (CropBox ∩ MediaBox) with `/Rotate` baked in,
        // matching the winit viewer and `PdfPage::effective_box()` invariant — the
        // raw `media_box` would show the untrimmed sheet and ignore page rotation.
        let mut interpreter = ContentInterpreter::new(page.effective_box())
            .with_page_rotation(page.rotate)
            .with_fonts(&mut fonts)
            .with_document(self.pdf.file(), &page.resources)
            .with_images(&mut images)
            .with_colors(&mut colors);
        if let Some(profile) = oi_cmyk {
            interpreter = interpreter.with_output_intent_cmyk(profile);
        }
        let display_list = interpreter.interpret(&content);

        let mut renderer = WgpuRenderer::new().with_fonts(&fonts).with_images(&images);
        if let Some(context) = context_slot.take() {
            renderer = renderer.with_context(context);
        }

        let render_result = renderer.render_display_list(&display_list, dpi / 72.0);
        *context_slot = renderer.take_context();

        let texture = render_result.map_err(|source| DocumentError::RenderPage {
            path: self.summary.path.clone(),
            page: index + 1,
            source,
        })?;

        let image = render_image_from_rgba(texture.width, texture.height, texture.data).ok_or(
            DocumentError::BuildPageImage {
                path: self.summary.path.clone(),
                page: index + 1,
            },
        )?;

        Ok(PagePreview {
            image,
            pixel_width: texture.width as f32,
            pixel_height: texture.height as f32,
        })
    }
}

fn render_image_from_rgba(width: u32, height: u32, mut rgba: Vec<u8>) -> Option<Arc<RenderImage>> {
    // GPUI's RenderImage expects BGRA bytes for direct render data.
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }

    let buffer = RgbaImage::from_raw(width, height, rgba)?;
    let frame = Frame::new(buffer);
    Some(Arc::new(RenderImage::new(SmallVec::from_elem(frame, 1))))
}
