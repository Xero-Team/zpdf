//! High-level PDF content extraction for text and rich document conversion.
//!
//! Text-only conversion deliberately omits an image cache, so image streams are
//! never decoded. Rich conversion retains successfully decoded page images; a
//! malformed or unsupported image is skipped by the content interpreter without
//! preventing text extraction.

use std::collections::HashMap;

use zpdf_color::IccCache;
use zpdf_content::interpreter::ContentInterpreter;
use zpdf_content::output_intent_cmyk_profile;
use zpdf_core::{Matrix, Result};
use zpdf_display_list::RenderCommand;
use zpdf_document::{DocInfo, PdfDocument, StructTree, XmpMetadata};
use zpdf_image::{DecodedImage, ImageCache};

/// Content retained by [`convert_pdf`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConversionMode {
    /// Extract Unicode text only. Image streams and other graphical content are
    /// not decoded.
    #[default]
    TextOnly,
    /// Extract text, decoded raster images, and document/page metadata.
    Rich,
}

/// Options for [`convert_pdf`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConversionOptions {
    pub mode: ConversionMode,
    /// Prefer a Tagged PDF's logical structure order and `/ActualText`/`/Alt`
    /// substitutions. Falls back to geometric reading order when unavailable.
    pub use_structure: bool,
}

/// One occurrence of an extracted image in page user space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImagePlacement {
    pub transform: Matrix,
    pub alpha: f32,
}

/// A unique decoded page image and every place it is drawn on that page.
#[derive(Debug, Clone)]
pub struct ConvertedImage {
    pub image: DecodedImage,
    pub placements: Vec<ImagePlacement>,
}

/// Extracted content and metadata for one PDF page.
#[derive(Debug, Clone)]
pub struct ConvertedPage {
    /// Zero-based physical page index in the source PDF.
    pub index: usize,
    /// Printed page label from `/PageLabels`, when present in rich mode.
    pub label: Option<String>,
    pub width_points: f64,
    pub height_points: f64,
    pub rotation: i32,
    pub text: String,
    /// Empty in text-only mode.
    pub images: Vec<ConvertedImage>,
}

/// Structured output from [`convert_pdf`], ready for a TXT, Markdown, or custom
/// serializer.
#[derive(Debug, Clone)]
pub struct ConvertedDocument {
    pub pdf_version: (u8, u8),
    pub total_pages: usize,
    /// Populated only in rich mode.
    pub info: Option<DocInfo>,
    /// Populated only in rich mode.
    pub xmp: Option<XmpMetadata>,
    pub pages: Vec<ConvertedPage>,
    /// Whether Tagged-PDF logical order was actually available and used.
    pub structure_order_used: bool,
}

/// Extract selected pages from an already-open PDF.
///
/// `page_indices` are zero-based and are emitted in the supplied order. An
/// invalid index returns the same page-range error as [`PdfDocument::page`]. In
/// [`ConversionMode::TextOnly`] the interpreter is intentionally constructed
/// without an [`ImageCache`], so pictures, shadings, and other graphics are
/// discarded without decode work. In rich mode, successfully decoded raster
/// images are returned once per page with all of their placements; image decode
/// failures remain non-fatal and text extraction continues. Retained images are
/// bounded across all selected pages by `ParseLimits::max_image_cache_bytes`,
/// while aggregate extracted text is bounded by `max_decoded_stream_bytes`.
pub fn convert_pdf(
    doc: &PdfDocument,
    page_indices: &[usize],
    options: ConversionOptions,
) -> Result<ConvertedDocument> {
    let rich = options.mode == ConversionMode::Rich;
    let struct_tree: Option<StructTree> = if options.use_structure {
        doc.struct_tree()
    } else {
        None
    };
    let page_labels = rich.then(|| doc.page_labels()).flatten();
    let oc_config = doc.oc_config();
    let doc_intents = if rich {
        doc.output_intents()
    } else {
        Vec::new()
    };
    let mut icc_cache = IccCache::new();
    let mut pages = Vec::with_capacity(page_indices.len());
    let limits = doc.file().limits();
    let mut retained_image_bytes = 0u64;
    let mut retained_text_bytes = 0u64;

    for &page_index in page_indices {
        let page = doc.page(page_index)?;
        let page_box = page.effective_box();
        let mut fonts = doc.load_page_fonts(&page);
        let content = doc.page_content_bytes(&page)?;
        let mut spans = Vec::new();
        let mut images = ImageCache::new();

        let output_intent = if rich {
            output_intent_cmyk_profile(
                doc.file(),
                doc.page_output_intents(&page),
                &doc_intents,
                &mut icc_cache,
            )
        } else {
            None
        };

        let mut interpreter = ContentInterpreter::new(page_box)
            .with_fonts(&mut fonts)
            .with_document(doc.file(), &page.resources)
            .with_text_sink(&mut spans)
            .with_operand_stack_limit(limits.max_operand_stack_depth as usize);
        if rich {
            let remaining_image_bytes = limits
                .max_image_cache_bytes
                .saturating_sub(retained_image_bytes);
            if remaining_image_bytes > 0 {
                interpreter = interpreter
                    .with_colors(&mut icc_cache)
                    .with_images(&mut images)
                    .with_image_cache_limit(remaining_image_bytes);
            }
        }
        if let Some(oc) = &oc_config {
            interpreter = interpreter.with_optional_content(oc);
        }
        if let Some(profile) = output_intent {
            interpreter = interpreter.with_output_intent_cmyk(profile);
        }
        let display_list = interpreter.interpret(&content);

        let text = match &struct_tree {
            Some(tree) => zpdf_content::text::struct_ordered_text(&spans, page_index, tree),
            None => zpdf_content::text::spans_to_text(spans, 2.0),
        };
        retained_text_bytes = retained_text_bytes
            .checked_add(u64::try_from(text.len()).unwrap_or(u64::MAX))
            .ok_or(zpdf_core::Error::StreamSizeLimit(
                limits.max_decoded_stream_bytes,
            ))?;
        if retained_text_bytes > limits.max_decoded_stream_bytes {
            return Err(zpdf_core::Error::StreamSizeLimit(
                limits.max_decoded_stream_bytes,
            ));
        }
        let converted_images = if rich {
            take_display_list_images(&display_list.commands, &mut images)
        } else {
            Vec::new()
        };
        retained_image_bytes = retained_image_bytes.saturating_add(
            converted_images
                .iter()
                .map(|image| u64::try_from(image.image.data.capacity()).unwrap_or(u64::MAX))
                .fold(0u64, u64::saturating_add),
        );

        pages.push(ConvertedPage {
            index: page_index,
            label: page_labels
                .as_ref()
                .and_then(|labels| labels.label(page_index)),
            width_points: page_box.width(),
            height_points: page_box.height(),
            rotation: page.rotate,
            text,
            images: converted_images,
        });
    }

    Ok(ConvertedDocument {
        pdf_version: doc.version(),
        total_pages: doc.page_count(),
        info: rich.then(|| doc.info()).flatten(),
        xmp: rich.then(|| doc.xmp_metadata()).flatten(),
        pages,
        structure_order_used: struct_tree
            .as_ref()
            .is_some_and(|tree| !tree.children.is_empty()),
    })
}

fn take_display_list_images(
    commands: &[RenderCommand],
    cache: &mut ImageCache,
) -> Vec<ConvertedImage> {
    let mut images = Vec::<ConvertedImage>::new();
    let mut positions = HashMap::<u32, usize>::new();

    for command in commands {
        let RenderCommand::DrawImage(draw) = command else {
            continue;
        };
        let converted_index = match positions.get(&draw.image_id).copied() {
            Some(index) => index,
            None => {
                let Some(image) = cache.remove(draw.image_id) else {
                    continue;
                };
                let index = images.len();
                images.push(ConvertedImage {
                    image,
                    placements: Vec::new(),
                });
                positions.insert(draw.image_id, index);
                index
            }
        };
        images[converted_index].placements.push(ImagePlacement {
            transform: draw.transform,
            alpha: draw.alpha,
        });
    }

    images
}
