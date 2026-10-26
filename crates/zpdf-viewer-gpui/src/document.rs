use std::path::PathBuf;
use std::sync::Arc;

use gpui::RenderImage;
use image::{Frame, RgbaImage};
use smallvec::SmallVec;
use thiserror::Error;
use zpdf::gpu::{GpuContext, WgpuRenderError, WgpuRenderer};
use zpdf::{InterpretedPage, PdfDocument, RenderBackend};

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
        let eb = page.effective_box_points();
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

    // Shared by preview rendering and headless tests. Reject known partial
    // pages before acquiring a GPU context or publishing an image.
    fn interpret_preview(&self, index: usize) -> Result<InterpretedPage, DocumentError> {
        let interpret = || {
            let page = zpdf::interpret_page(&self.pdf, index, Default::default())?;
            page.ensure_complete()?;
            Ok(page)
        };
        interpret().map_err(|source| DocumentError::ReadPageContent {
            path: self.summary.path.clone(),
            page: index + 1,
            source,
        })
    }

    pub fn render_page_preview(
        &self,
        index: usize,
        dpi: f32,
        context_slot: &mut Option<GpuContext>,
    ) -> Result<PagePreview, DocumentError> {
        let InterpretedPage {
            display_list,
            fonts,
            images,
            ..
        } = self.interpret_preview(index)?;

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
    for pixel in rgba.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }

    let buffer = RgbaImage::from_raw(width, height, rgba)?;
    let frame = Frame::new(buffer);
    Some(Arc::new(RenderImage::new(SmallVec::from_elem(frame, 1))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zpdf::{ObjectId, PdfDict, PdfName, PdfObject, Rect};
    use zpdf_writer::{AnnotationSpec, DocumentBuilder, IncrementalWriter};

    fn fixture(content: &[u8]) -> LoadedDocument {
        let mut builder = DocumentBuilder::new();
        builder.add_page(120.0, 80.0);
        let mut writer = IncrementalWriter::new(builder.build().unwrap()).unwrap();
        writer
            .add_annotation(
                0,
                &AnnotationSpec::Square {
                    rect: Rect::new(40.0, 20.0, 80.0, 60.0),
                    color: (1.0, 0.0, 0.0),
                    interior: Some((1.0, 0.0, 0.0)),
                    width: 0.0,
                },
            )
            .unwrap();
        let mut group = PdfDict::new();
        group.insert(PdfName::new("Type"), PdfObject::Name(PdfName::new("OCG")));
        let (n, g) = writer.try_add_object(&PdfObject::Dict(group)).unwrap();
        let group = ObjectId(n, g as u16);
        let mut config = PdfDict::new();
        config.insert(
            PdfName::new("OFF"),
            PdfObject::Array(vec![PdfObject::Ref(group)]),
        );
        let mut oc = PdfDict::new();
        oc.insert(PdfName::new("D"), PdfObject::Dict(config));
        let root = writer.document().file().trailer.get_ref("Root").unwrap();
        let mut catalog = writer
            .resolve_current(root)
            .unwrap()
            .as_dict()
            .unwrap()
            .clone();
        catalog.insert(PdfName::new("OCProperties"), PdfObject::Dict(oc));
        writer.overwrite_object(root, PdfObject::Dict(catalog));
        let page = writer.document().page(0).unwrap();
        let mut page_dict = writer
            .resolve_current(page.id)
            .unwrap()
            .as_dict()
            .unwrap()
            .clone();
        let mut properties = PdfDict::new();
        properties.insert(PdfName::new("Layer"), PdfObject::Ref(group));
        let mut resources = PdfDict::new();
        resources.insert(PdfName::new("Properties"), PdfObject::Dict(properties));
        page_dict.insert(PdfName::new("Resources"), PdfObject::Dict(resources));
        page_dict.insert(PdfName::new("UserUnit"), PdfObject::Integer(2));
        page_dict.insert(PdfName::new("Rotate"), PdfObject::Integer(90));
        let (n, g) = writer.try_add_stream(&PdfDict::new(), content).unwrap();
        page_dict.insert(
            PdfName::new("Contents"),
            PdfObject::Ref(ObjectId(n, g as u16)),
        );
        writer.overwrite_object(page.id, PdfObject::Dict(page_dict));
        let mut bytes = std::io::Cursor::new(Vec::new());
        writer.write(&mut bytes).unwrap();
        let path = std::env::temp_dir().join(format!(
            "zpdf-preview-{}-{}.pdf",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, bytes.into_inner()).unwrap();
        let document = load_document(path.clone()).unwrap();
        std::fs::remove_file(path).unwrap();
        document
    }

    #[test]
    fn preview_honors_layers_annotations_rotation_and_user_units() {
        let doc = fixture(b"/OC /Layer BDC 0 0 1 rg 0 0 120 80 re f EMC");
        assert_eq!(
            (doc.summary.pages[0].width, doc.summary.pages[0].height),
            (240.0, 160.0)
        );
        let page = doc.interpret_preview(0).unwrap();
        let pixels = zpdf::cpu::CpuRenderer::new()
            .with_fonts(&page.fonts)
            .with_images(&page.images)
            .render_display_list(&page.display_list, 1.0)
            .unwrap();
        assert_eq!((pixels.width, pixels.height), (160, 240));
        let center = (120 * pixels.width as usize + 80) * 4;
        assert_eq!(&pixels.data[center..center + 4], &[255, 0, 0, 255]);
        assert_eq!(&pixels.data[..4], &[255, 255, 255, 255]);
    }

    #[test]
    fn incomplete_preview_is_rejected_before_rendering() {
        let doc = fixture(b"/Missing Do");
        assert!(matches!(
            doc.interpret_preview(0),
            Err(DocumentError::ReadPageContent {
                source: zpdf::Error::IncompleteOutput(_),
                ..
            })
        ));
    }
}
