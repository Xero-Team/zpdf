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
    #[error("PDF {path} contains no pages")]
    EmptyDocument { path: PathBuf },
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
    #[error("GPU rendering failed and CPU fallback failed for page {page} from {path}: {source}")]
    RenderPageCpu {
        path: PathBuf,
        page: usize,
        #[source]
        source: zpdf::cpu::CpuRenderError,
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
    if pdf.page_count() == 0 {
        return Err(DocumentError::EmptyDocument { path });
    }

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
    pub fn document(&self) -> &PdfDocument {
        &self.pdf
    }

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
            .with_colors(&mut colors)
            .with_operand_stack_limit(self.pdf.file().limits().max_operand_stack_depth as usize);
        if let Some(ref profile) = oi_cmyk {
            interpreter = interpreter.with_output_intent_cmyk(profile.clone());
        }
        let mut display_list = interpreter.interpret(&content);

        // Render annotations on top of page content
        let annotations = self.pdf.page_annotations(&page);
        for annot in annotations {
            // Skip hidden or no-view annotations
            if (annot.flags & 0x02) != 0 || (annot.flags & 0x20) != 0 {
                continue;
            }

            // Render the annotation's appearance stream
            if let Some(appearance_id) = annot.appearance {
                // Try to decode the appearance stream
                match self.pdf.file().resolve_stream_data(appearance_id) {
                    Ok(appearance_content) => {
                        // Create an interpreter for the annotation appearance
                        let mut annot_interp = ContentInterpreter::new(annot.rect)
                            .with_fonts(&mut fonts)
                            .with_document(self.pdf.file(), &page.resources)
                            .with_images(&mut images)
                            .with_colors(&mut colors)
                            .with_operand_stack_limit(
                                self.pdf.file().limits().max_operand_stack_depth as usize,
                            );
                        if let Some(ref profile) = oi_cmyk {
                            annot_interp = annot_interp.with_output_intent_cmyk(profile.clone());
                        }

                        let annot_display_list = annot_interp.interpret(&appearance_content);

                        // Merge annotation display list commands into main display list
                        display_list.commands.extend(annot_display_list.commands);
                    }
                    Err(e) => {
                        // Skip annotations with invalid appearance streams
                        tracing::warn!(
                            "Failed to decode annotation appearance {}: {}",
                            appearance_id,
                            e
                        );
                    }
                }
            }
        }

        let mut renderer = WgpuRenderer::new()
            .with_limits(self.pdf.file().limits())
            .with_fonts(&fonts)
            .with_images(&images);
        if let Some(context) = context_slot.take() {
            renderer = renderer.with_context(context);
        }

        let render_result = renderer.render_display_list(&display_list, dpi / 72.0);
        let context = renderer.take_context();
        let (width, height, data) = match render_result {
            Ok(texture) => {
                *context_slot = context;
                (texture.width, texture.height, texture.data)
            }
            Err(source) => {
                // Validation/size failures leave the device healthy. Poll,
                // readback, or uncaptured device errors may indicate loss, so
                // discard that context and recreate it on the next attempt.
                *context_slot = if matches!(
                    source,
                    WgpuRenderError::Unsupported(_)
                        | WgpuRenderError::InvalidPage(_)
                        | WgpuRenderError::NoActivePage
                ) {
                    context
                } else {
                    None
                };
                tracing::warn!(page = index + 1, error = %source, "GPU render failed; using CPU fallback");
                let page = zpdf::cpu::CpuRenderer::new()
                    .with_limits(self.pdf.file().limits())
                    .with_fonts(&fonts)
                    .with_images(&images)
                    .render_display_list(&display_list, dpi / 72.0)
                    .map_err(|source| DocumentError::RenderPageCpu {
                        path: self.summary.path.clone(),
                        page: index + 1,
                        source,
                    })?;
                (page.width, page.height, page.data)
            }
        };

        let image =
            render_image_from_rgba(width, height, data).ok_or(DocumentError::BuildPageImage {
                path: self.summary.path.clone(),
                page: index + 1,
            })?;

        Ok(PagePreview {
            image,
            pixel_width: width as f32,
            pixel_height: height as f32,
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
