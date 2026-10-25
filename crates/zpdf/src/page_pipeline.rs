//! Complete page semantics shared by library consumers and export frontends.
use crate::{
    ContentInterpreter, DisplayList, FontCache, IccCache, ImageCache, InterpretStats, PdfDocument,
    Result,
};

#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct PageInterpretOptions {
    /// Include stored and synthesized annotation/form appearances.
    pub include_annotations: bool,
    /// Optional destination raster scale for decoded-image downsampling.
    pub image_downscale: Option<f32>,
}
impl Default for PageInterpretOptions {
    fn default() -> Self {
        Self {
            include_annotations: true,
            image_downscale: None,
        }
    }
}

/// Owned drawing resources. Keep these alive with the display list.
#[non_exhaustive]
pub struct InterpretedPage {
    pub display_list: DisplayList,
    pub fonts: FontCache,
    pub images: ImageCache,
    pub stats: InterpretStats,
    pub content_bytes: usize,
}

/// Interpret a page with rotation, annotations, default layer visibility,
/// output intents, font fallback and the document's limits applied.
pub fn interpret_page(
    doc: &PdfDocument,
    index: usize,
    options: PageInterpretOptions,
) -> Result<InterpretedPage> {
    interpret_page_with_cache(doc, index, options, &mut IccCache::new())
}

/// Like [`interpret_page`], reusing ICC transforms across pages.
pub fn interpret_page_with_cache(
    doc: &PdfDocument,
    index: usize,
    options: PageInterpretOptions,
    colors: &mut IccCache,
) -> Result<InterpretedPage> {
    let page = doc.page(index)?;
    let content = doc.page_content_bytes(&page)?;
    let mut fonts = doc.load_page_fonts(&page);
    let mut images = ImageCache::new();
    let annotations = if options.include_annotations {
        doc.page_annotations(&page)
    } else {
        Vec::new()
    };
    let layers = doc.oc_config();
    let output_intent = crate::output_intent_cmyk_profile(
        doc.file(),
        doc.page_output_intents(&page),
        &doc.output_intents(),
        colors,
    );
    let mut interpreter = ContentInterpreter::new(page.effective_box())
        .with_page_rotation(page.rotate)
        .with_document(doc.file(), &page.resources)
        .with_fonts(&mut fonts)
        .with_shared_fonts(doc)
        .with_images(&mut images)
        .with_colors(colors)
        .with_annotations(&annotations);
    if let Some(layers) = &layers {
        interpreter = interpreter.with_optional_content(layers);
    }
    if let Some(profile) = output_intent {
        interpreter = interpreter.with_output_intent_cmyk(profile);
    }
    if let Some(scale) = options.image_downscale {
        interpreter = interpreter.with_image_downscale(scale);
    }
    let (display_list, stats) = interpreter.interpret_with_stats(&content);
    Ok(InterpretedPage {
        display_list,
        fonts,
        images,
        stats,
        content_bytes: content.len(),
    })
}
