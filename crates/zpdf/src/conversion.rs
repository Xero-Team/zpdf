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
use zpdf_core::{Matrix, Point, Rect, Result};
use zpdf_display_list::{
    DisplayList, GlyphRun, LineJoin, Paint, Path, RenderCommand, StrokeStyle, TilingPattern,
};
use zpdf_document::{DocInfo, PdfDocument, XmpMetadata};
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

/// One occurrence of an extracted image in unrotated page space, in points.
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
    /// Known omissions/truncation for this page and the requested mode.
    pub diagnostics: zpdf_core::ProcessingDiagnostics,
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

impl ConvertedDocument {
    pub fn diagnostics(&self) -> zpdf_core::ProcessingDiagnostics {
        let mut all = zpdf_core::ProcessingDiagnostics::default();
        for page in &self.pages {
            all.merge(page.diagnostics);
        }
        all
    }
    pub fn is_complete(&self) -> bool {
        self.diagnostics().is_text_complete()
    }
    pub fn ensure_complete(&self) -> Result<()> {
        self.diagnostics().ensure_text_complete()
    }
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
    let (struct_tree, struct_streams) = if options.use_structure {
        match doc.struct_tree_with_streams() {
            Some((tree, streams)) => (Some(tree), streams),
            None => (None, zpdf_document::structure::StructTreeStreams::default()),
        }
    } else {
        (None, zpdf_document::structure::StructTreeStreams::default())
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
        let (content, mut diagnostics) = doc.page_content_with_diagnostics(&page)?;
        let mut spans = Vec::new();
        let mut span_streams = Vec::new();
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
            .with_page_user_unit(page.user_unit)
            .with_fonts(&mut fonts)
            .with_document(doc.file(), &page.resources)
            .with_shared_fonts(doc)
            .with_text_sink(&mut spans)
            .with_operand_stack_limit(limits.max_operand_stack_depth as usize);
        if rich {
            let remaining_image_bytes = limits
                .max_image_cache_bytes
                .saturating_sub(retained_image_bytes);
            interpreter = interpreter
                .with_colors(&mut icc_cache)
                .with_images(&mut images)
                .with_image_cache_limit(remaining_image_bytes);
        }
        if let Some(oc) = &oc_config {
            interpreter = interpreter.with_optional_content(oc);
        }
        if struct_tree.is_some() {
            interpreter = interpreter.with_span_streams(&mut span_streams);
        }
        if let Some(mcids) = struct_streams.actual_text_mcids.get(&page_index) {
            interpreter = interpreter.with_struct_actual_text_mcids(mcids.clone());
        }
        if let Some(profile) = output_intent {
            interpreter = interpreter.with_output_intent_cmyk(profile);
        }
        let (display_list, stats) = interpreter.interpret_with_stats(&content);
        diagnostics.merge(stats.diagnostics);
        diagnostics.set_page(page_index);

        let text = match &struct_tree {
            Some(tree) => zpdf_content::text::struct_ordered_text_with_streams(
                &spans,
                &span_streams,
                page_index,
                tree,
                &struct_streams.kid_streams,
            ),
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
            take_display_list_images(&display_list, &mut images, &mut diagnostics)
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
            diagnostics,
            index: page_index,
            label: page_labels
                .as_ref()
                .and_then(|labels| labels.label(page_index)),
            width_points: page_box.width() * page.user_unit,
            height_points: page_box.height() * page.user_unit,
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

const MAX_TILING_IMAGE_DEPTH: u8 = 8;
const MAX_TILING_IMAGE_PLACEMENTS: usize = 4096;
const MAX_TILING_IMAGE_VISITS: usize = 4096;
const MAX_TILING_IMAGE_SCAN_COMMANDS: usize = 65_536;
const MAX_TILING_IMAGE_REPLAY_COMMANDS: usize = 65_536;

struct ImageCollector<'a> {
    cache: &'a mut ImageCache,
    images: Vec<ConvertedImage>,
    positions: HashMap<u32, usize>,
    remaining_tiling_placements: usize,
    remaining_tiling_visits: usize,
    remaining_image_scan_cmds: usize,
    remaining_tiling_replay_cmds: usize,
    image_scan_exhausted: bool,
    /// Memoized `dl_contains_images` results keyed by `(Arc::as_ptr, depth)`
    /// so shared image-free child cells in a nested DAG are scanned at most
    /// once per remaining depth.
    tiling_has_images: HashMap<(usize, u8), bool>,
    truncated: bool,
}

impl<'a> ImageCollector<'a> {
    fn record(&mut self, image_id: u32, transform: Matrix, alpha: f32, from_tiling: bool) {
        if from_tiling {
            if self.remaining_tiling_placements == 0 {
                self.truncated = true;
                return;
            }
            self.remaining_tiling_placements -= 1;
        }
        let converted_index = match self.positions.get(&image_id).copied() {
            Some(index) => index,
            None => {
                let Some(image) = self.cache.remove(image_id) else {
                    return;
                };
                let index = self.images.len();
                self.images.push(ConvertedImage {
                    image,
                    placements: Vec::new(),
                });
                self.positions.insert(image_id, index);
                index
            }
        };
        self.images[converted_index]
            .placements
            .push(ImagePlacement { transform, alpha });
    }
}

fn path_bounds(path: &Path) -> Option<Rect> {
    let (mut x0, mut y0, mut x1, mut y1) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for p in path.points() {
        if !p.x.is_finite() || !p.y.is_finite() {
            return None;
        }
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    (x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite())
        .then(|| Rect::new(x0, y0, x1, y1))
}

fn stroke_bounds(path: &Path, style: &StrokeStyle) -> Option<Rect> {
    let b = path_bounds(path)?;
    let reach = match style.join {
        LineJoin::Miter => style.miter_limit.clamp(1.0, 100.0),
        _ => std::f32::consts::SQRT_2,
    };
    let grow =
        (style.width.max(1.0) as f64 * 0.5 * reach.max(std::f32::consts::SQRT_2) as f64).max(0.5);
    Some(Rect::new(
        b.x0 - grow,
        b.y0 - grow,
        b.x1 + grow,
        b.y1 + grow,
    ))
}

fn glyph_run_bounds(run: &GlyphRun) -> Option<Rect> {
    if run.glyphs.is_empty() || !run.font_size.is_finite() || run.font_size <= 0.0 {
        return None;
    }
    let (mut x0, mut y0, mut x1, mut y1) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    let h = run.font_size as f64;
    for g in &run.glyphs {
        let gx = g.x as f64;
        let gy = g.y as f64;
        let adv = (g.advance as f64).abs().max(h);
        x0 = x0.min(gx);
        x1 = x1.max(gx + adv);
        y0 = y0.min(gy - h);
        y1 = y1.max(gy + h);
    }
    transform_rect(Rect::new(x0, y0, x1, y1), &run.transform)
}

fn transform_rect(rect: Rect, m: &Matrix) -> Option<Rect> {
    let r = rect.normalize();
    let corners = [
        Point::new(r.x0, r.y0),
        Point::new(r.x1, r.y0),
        Point::new(r.x0, r.y1),
        Point::new(r.x1, r.y1),
    ];
    let (mut x0, mut y0) = (f64::INFINITY, f64::INFINITY);
    let (mut x1, mut y1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for c in &corners {
        let p = c.transform(m);
        if !p.x.is_finite() || !p.y.is_finite() {
            return None;
        }
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    Some(Rect::new(x0, y0, x1, y1))
}

fn intersect_rect(a: Rect, b: Rect) -> Option<Rect> {
    let a = a.normalize();
    let b = b.normalize();
    let x0 = a.x0.max(b.x0);
    let y0 = a.y0.max(b.y0);
    let x1 = a.x1.min(b.x1);
    let y1 = a.y1.min(b.y1);
    (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, x1, y1))
}

fn dl_contains_images(
    dl: &std::sync::Arc<DisplayList>,
    depth: u8,
    collector: &mut ImageCollector<'_>,
) -> bool {
    if depth > MAX_TILING_IMAGE_DEPTH {
        // Conservatively treat depth-capped subtrees as image-bearing so the
        // caller signals truncation rather than silently skipping them.
        return true;
    }
    let key = (std::sync::Arc::as_ptr(dl) as *const () as usize, depth);
    if let Some(&cached) = collector.tiling_has_images.get(&key) {
        return cached;
    }
    let mut found = false;
    for cmd in &dl.commands {
        if collector.remaining_image_scan_cmds == 0 {
            collector.image_scan_exhausted = true;
            collector.truncated = true;
            return false;
        }
        collector.remaining_image_scan_cmds -= 1;
        match cmd {
            RenderCommand::DrawImage(_) => {
                found = true;
                break;
            }
            RenderCommand::FillPath {
                paint: Paint::Tiling(t),
                ..
            }
            | RenderCommand::StrokePath {
                paint: Paint::Tiling(t),
                ..
            } => {
                if dl_contains_images(&t.commands, depth + 1, collector) {
                    found = true;
                    break;
                }
                if collector.image_scan_exhausted {
                    return false;
                }
            }
            RenderCommand::DrawGlyphRun(run) => {
                if let Paint::Tiling(t) = &run.paint {
                    if dl_contains_images(&t.commands, depth + 1, collector) {
                        found = true;
                        break;
                    }
                    if collector.image_scan_exhausted {
                        return false;
                    }
                }
            }
            _ => {}
        }
    }
    collector.tiling_has_images.insert(key, found);
    found
}

fn collect_tiling_images(
    tiling: &TilingPattern,
    shape_page_bounds: Rect,
    ctm: &Matrix,
    clip: Option<Rect>,
    outer_alpha: f32,
    depth: u8,
    collector: &mut ImageCollector<'_>,
) {
    let Some(clip_rect) = clip else {
        return;
    };
    let Some(bounds) = intersect_rect(shape_page_bounds, clip_rect) else {
        return;
    };
    if !dl_contains_images(&tiling.commands, depth + 1, collector) {
        return;
    }
    if depth >= MAX_TILING_IMAGE_DEPTH {
        collector.truncated = true;
        return;
    }
    let pat_to_page = ctm.concat(&tiling.matrix);
    let Some(from_page) = pat_to_page.inverse() else {
        return;
    };
    let Some(pat_bounds) = transform_rect(bounds, &from_page) else {
        return;
    };
    let bbox = tiling.bbox.normalize();
    if ![
        bbox.x0,
        bbox.y0,
        bbox.x1,
        bbox.y1,
        tiling.x_step,
        tiling.y_step,
    ]
    .iter()
    .all(|v| v.is_finite())
        || bbox.width() <= 0.0
        || bbox.height() <= 0.0
        || tiling.x_step == 0.0
        || tiling.y_step == 0.0
    {
        return;
    }
    let range = |lo: f64, hi: f64, cell_lo: f64, cell_hi: f64, step: f64| -> (i64, i64) {
        let t0 = (lo - cell_hi) / step;
        let t1 = (hi - cell_lo) / step;
        let (a, b) = if t0 <= t1 { (t0, t1) } else { (t1, t0) };
        (
            (a.floor() as i64).saturating_add(1),
            (b.ceil() as i64).saturating_sub(1),
        )
    };
    let (i0, i1) = range(
        pat_bounds.x0,
        pat_bounds.x1,
        bbox.x0,
        bbox.x1,
        tiling.x_step,
    );
    let (j0, j1) = range(
        pat_bounds.y0,
        pat_bounds.y1,
        bbox.y0,
        bbox.y1,
        tiling.y_step,
    );
    if i0 > i1 || j0 > j1 {
        return;
    }

    'tiles: for j in j0..=j1 {
        for i in i0..=i1 {
            if collector.remaining_tiling_placements == 0
                || collector.remaining_tiling_visits == 0
                || collector.remaining_tiling_replay_cmds == 0
            {
                collector.truncated = true;
                break 'tiles;
            }
            collector.remaining_tiling_visits -= 1;
            let tile_ctm = pat_to_page.concat(&Matrix::translate(
                i as f64 * tiling.x_step,
                j as f64 * tiling.y_step,
            ));
            let Some(tile_clip) =
                transform_rect(bbox, &tile_ctm).and_then(|r| intersect_rect(bounds, r))
            else {
                continue;
            };
            collect_images_recursive(
                &tiling.commands.commands,
                &tile_ctm,
                Some(tile_clip),
                outer_alpha,
                depth + 1,
                collector,
            );
        }
    }
}

fn collect_images_recursive(
    commands: &[RenderCommand],
    ctm: &Matrix,
    initial_clip: Option<Rect>,
    outer_alpha: f32,
    depth: u8,
    collector: &mut ImageCollector<'_>,
) {
    let mut clip_stack: Vec<Option<Rect>> = vec![initial_clip];

    for command in commands {
        if depth > 0 {
            if collector.remaining_tiling_replay_cmds == 0 {
                collector.truncated = true;
                break;
            }
            collector.remaining_tiling_replay_cmds -= 1;
        }
        let active_clip = *clip_stack.last().unwrap_or(&initial_clip);
        match command {
            RenderCommand::PushClip { path, .. } => {
                let next = if path.is_empty() {
                    None
                } else if let Some(b) = path_bounds(path).and_then(|r| transform_rect(r, ctm)) {
                    active_clip.and_then(|c| intersect_rect(c, b))
                } else {
                    active_clip
                };
                clip_stack.push(next);
            }
            RenderCommand::PushClipStroke { path, style } => {
                let next = if path.is_empty() {
                    None
                } else if let Some(b) =
                    stroke_bounds(path, style).and_then(|r| transform_rect(r, ctm))
                {
                    active_clip.and_then(|c| intersect_rect(c, b))
                } else {
                    active_clip
                };
                clip_stack.push(next);
            }
            RenderCommand::PopClip => {
                if clip_stack.len() > 1 {
                    clip_stack.pop();
                }
            }
            RenderCommand::DrawImage(draw) => {
                if depth > 0 && active_clip.is_none() {
                    continue;
                }
                let transform = ctm.concat(&draw.transform);
                let alpha = if draw.alpha.is_finite() {
                    (outer_alpha * draw.alpha).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                collector.record(draw.image_id, transform, alpha, depth > 0);
            }
            RenderCommand::FillPath {
                path,
                paint: Paint::Tiling(tiling),
                alpha,
                overprint,
                ..
            } => {
                if overprint.is_some_and(|op| op.active == 0) {
                    continue;
                }
                let eff_alpha = if alpha.is_finite() {
                    (outer_alpha * *alpha).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                if let Some(bounds) = path_bounds(path).and_then(|r| transform_rect(r, ctm)) {
                    collect_tiling_images(
                        tiling,
                        bounds,
                        ctm,
                        active_clip,
                        eff_alpha,
                        depth,
                        collector,
                    );
                }
            }
            RenderCommand::StrokePath {
                path,
                style,
                paint: Paint::Tiling(tiling),
                alpha,
                overprint,
            } => {
                if overprint.is_some_and(|op| op.active == 0) {
                    continue;
                }
                let eff_alpha = if alpha.is_finite() {
                    (outer_alpha * *alpha).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                if let Some(bounds) =
                    stroke_bounds(path, style).and_then(|r| transform_rect(r, ctm))
                {
                    collect_tiling_images(
                        tiling,
                        bounds,
                        ctm,
                        active_clip,
                        eff_alpha,
                        depth,
                        collector,
                    );
                }
            }
            RenderCommand::DrawGlyphRun(run) => {
                if run.overprint.is_some_and(|op| op.active == 0) {
                    continue;
                }
                if let Paint::Tiling(tiling) = &run.paint {
                    let eff_alpha = if run.alpha.is_finite() {
                        (outer_alpha * run.alpha).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    if let Some(bounds) = glyph_run_bounds(run).and_then(|r| transform_rect(r, ctm))
                    {
                        collect_tiling_images(
                            tiling,
                            bounds,
                            ctm,
                            active_clip,
                            eff_alpha,
                            depth,
                            collector,
                        );
                    }
                }
            }
            _ => {}
        }
    }
}

fn take_display_list_images(
    display_list: &DisplayList,
    cache: &mut ImageCache,
    diagnostics: &mut zpdf_core::ProcessingDiagnostics,
) -> Vec<ConvertedImage> {
    let mut collector = ImageCollector {
        cache,
        images: Vec::new(),
        positions: HashMap::new(),
        remaining_tiling_placements: MAX_TILING_IMAGE_PLACEMENTS,
        remaining_tiling_visits: MAX_TILING_IMAGE_VISITS,
        remaining_image_scan_cmds: MAX_TILING_IMAGE_SCAN_COMMANDS,
        remaining_tiling_replay_cmds: MAX_TILING_IMAGE_REPLAY_COMMANDS,
        image_scan_exhausted: false,
        tiling_has_images: HashMap::new(),
        truncated: false,
    };
    let page_rect = display_list.page_rect.normalize();
    let initial_clip = (page_rect.width() > 0.0 && page_rect.height() > 0.0).then_some(page_rect);
    collect_images_recursive(
        &display_list.commands,
        &Matrix::identity(),
        initial_clip,
        1.0,
        0,
        &mut collector,
    );
    if collector.truncated {
        diagnostics.resource_limit_reached = true;
    }
    collector.images
}

#[cfg(test)]
mod tiling_image_scan_tests {
    use super::*;
    use std::sync::Arc;
    use zpdf_display_list::{Color, FillRule};

    #[test]
    fn image_free_nested_tiling_dag_memoizes_without_exponential_rescan() {
        // Build a 7-level diamond DAG where every level references the same
        // image-free child Arc<DisplayList> 16 times (16^7 = 268M naive paths).
        let mut leaf_dl = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        let mut sq = Path::new();
        sq.rect(Rect::new(0.0, 0.0, 10.0, 10.0));
        leaf_dl.push(RenderCommand::FillPath {
            path: sq.clone(),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        let mut child_arc = Arc::new(leaf_dl);
        for _ in 0..7 {
            let child_tiling = Arc::new(TilingPattern {
                commands: Arc::clone(&child_arc),
                bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
                x_step: 10.0,
                y_step: 10.0,
                matrix: Matrix::identity(),
                tiling_type: 1,
            });
            let mut parent_dl = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
            for _ in 0..16 {
                parent_dl.push(RenderCommand::FillPath {
                    path: sq.clone(),
                    rule: FillRule::NonZero,
                    paint: Paint::Tiling(Arc::clone(&child_tiling)),
                    alpha: 1.0,
                    overprint: None,
                });
            }
            child_arc = Arc::new(parent_dl);
        }

        let root_tiling = Arc::new(TilingPattern {
            commands: child_arc,
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        });
        let mut page_dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        let mut fill = Path::new();
        fill.rect(Rect::new(0.0, 0.0, 100.0, 100.0));
        page_dl.push(RenderCommand::FillPath {
            path: fill,
            rule: FillRule::NonZero,
            paint: Paint::Tiling(root_tiling),
            alpha: 1.0,
            overprint: None,
        });

        let mut cache = ImageCache::new();
        let mut diagnostics = zpdf_core::ProcessingDiagnostics::default();
        let images = take_display_list_images(&page_dl, &mut cache, &mut diagnostics);
        assert!(images.is_empty());
        assert!(
            !diagnostics.resource_limit_reached,
            "memoized image-free DAG should scan each shared list once without hitting limits"
        );
    }

    #[test]
    fn image_presence_scan_budget_reports_truncation_when_exhausted() {
        let mut cell_dl = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        let mut sq = Path::new();
        sq.rect(Rect::new(0.0, 0.0, 1.0, 1.0));
        for _ in 0..100 {
            cell_dl.push(RenderCommand::FillPath {
                path: sq.clone(),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::black()),
                alpha: 1.0,
                overprint: None,
            });
        }
        let tiling = Arc::new(TilingPattern {
            commands: Arc::new(cell_dl),
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        });
        let mut cache = ImageCache::new();
        let mut collector = ImageCollector {
            cache: &mut cache,
            images: Vec::new(),
            positions: HashMap::new(),
            remaining_tiling_placements: MAX_TILING_IMAGE_PLACEMENTS,
            remaining_tiling_visits: MAX_TILING_IMAGE_VISITS,
            remaining_image_scan_cmds: 10,
            remaining_tiling_replay_cmds: MAX_TILING_IMAGE_REPLAY_COMMANDS,
            image_scan_exhausted: false,
            tiling_has_images: HashMap::new(),
            truncated: false,
        };
        assert!(!dl_contains_images(&tiling.commands, 1, &mut collector));
        assert!(
            collector.truncated,
            "exhausting scan budget must flag truncation"
        );
    }

    #[test]
    fn image_presence_scan_flags_truncation_after_exact_child_budget_boundary() {
        let mut sq = Path::new();
        sq.rect(Rect::new(0.0, 0.0, 1.0, 1.0));
        let mut child_dl = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        child_dl.push(RenderCommand::FillPath {
            path: sq.clone(),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        let child = Arc::new(TilingPattern {
            commands: Arc::new(child_dl),
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        });
        let mut parent_dl = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        parent_dl.push(RenderCommand::FillPath {
            path: sq.clone(),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(child),
            alpha: 1.0,
            overprint: None,
        });
        parent_dl.push(RenderCommand::FillPath {
            path: sq,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        let parent_arc = Arc::new(parent_dl);
        let mut cache = ImageCache::new();
        let mut collector = ImageCollector {
            cache: &mut cache,
            images: Vec::new(),
            positions: HashMap::new(),
            remaining_tiling_placements: MAX_TILING_IMAGE_PLACEMENTS,
            remaining_tiling_visits: MAX_TILING_IMAGE_VISITS,
            // Exactly enough to visit parent[0] and child[0], leaving 0 for parent[1].
            remaining_image_scan_cmds: 2,
            remaining_tiling_replay_cmds: MAX_TILING_IMAGE_REPLAY_COMMANDS,
            image_scan_exhausted: false,
            tiling_has_images: HashMap::new(),
            truncated: false,
        };
        assert!(!dl_contains_images(&parent_arc, 1, &mut collector));
        assert!(
            collector.truncated,
            "trailing sibling command after exact child budget consumption must flag truncation"
        );
    }

    #[test]
    fn clipped_out_tiling_skips_image_presence_scan() {
        let mut cell_dl = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        let mut sq = Path::new();
        sq.rect(Rect::new(0.0, 0.0, 1.0, 1.0));
        for _ in 0..500 {
            cell_dl.push(RenderCommand::FillPath {
                path: sq.clone(),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::black()),
                alpha: 1.0,
                overprint: None,
            });
        }
        let tiling = Arc::new(TilingPattern {
            commands: Arc::new(cell_dl),
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        });
        let mut cache = ImageCache::new();
        let mut collector = ImageCollector {
            cache: &mut cache,
            images: Vec::new(),
            positions: HashMap::new(),
            remaining_tiling_placements: MAX_TILING_IMAGE_PLACEMENTS,
            remaining_tiling_visits: MAX_TILING_IMAGE_VISITS,
            remaining_image_scan_cmds: 100,
            remaining_tiling_replay_cmds: MAX_TILING_IMAGE_REPLAY_COMMANDS,
            image_scan_exhausted: false,
            tiling_has_images: HashMap::new(),
            truncated: false,
        };
        // Shape [200..300] is disjoint from clip [0..100].
        collect_tiling_images(
            &tiling,
            Rect::new(200.0, 200.0, 300.0, 300.0),
            &Matrix::identity(),
            Some(Rect::new(0.0, 0.0, 100.0, 100.0)),
            1.0,
            0,
            &mut collector,
        );
        assert_eq!(
            collector.remaining_image_scan_cmds, 100,
            "disjoint clip must skip dl_contains_images without spending scan budget"
        );
        assert!(!collector.truncated);
    }

    #[test]
    fn dense_large_cell_tile_replay_is_bounded_and_preserves_later_page_images() {
        let mut cache = ImageCache::new();
        let make_img = |r: u8| zpdf_image::DecodedImage {
            width: 1,
            height: 1,
            data: vec![r, 0, 0, 255],
            has_alpha: false,
            is_image_mask: false,
            premultiplied: false,
        };
        let cell_img_id = cache.insert(make_img(255));
        let page_img_id = cache.insert(make_img(42));

        // Cell with 30,000 ordinary fills + 1 image draw.
        let mut cell_dl = DisplayList::new(Rect::new(0.0, 0.0, 2.0, 2.0));
        let mut sq = Path::new();
        sq.rect(Rect::new(0.0, 0.0, 1.0, 1.0));
        for _ in 0..30_000 {
            cell_dl.push(RenderCommand::FillPath {
                path: sq.clone(),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::black()),
                alpha: 1.0,
                overprint: None,
            });
        }
        cell_dl.push(RenderCommand::DrawImage(zpdf_display_list::ImageDraw {
            image_id: cell_img_id,
            transform: Matrix::scale(1.0, 1.0),
            alpha: 1.0,
            is_image_mask: false,
        }));
        let tiling = Arc::new(TilingPattern {
            commands: Arc::new(cell_dl),
            bbox: Rect::new(0.0, 0.0, 2.0, 2.0),
            x_step: 2.0,
            y_step: 2.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        });

        // 64×64 = 4,096 tiles (fits inside MAX_TILING_IMAGE_VISITS), followed by
        // a direct page image that must still be extracted after tiled replay caps out.
        let mut page_dl = DisplayList::new(Rect::new(0.0, 0.0, 128.0, 128.0));
        let mut fill = Path::new();
        fill.rect(Rect::new(0.0, 0.0, 128.0, 128.0));
        page_dl.push(RenderCommand::FillPath {
            path: fill,
            rule: FillRule::NonZero,
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
        });
        page_dl.push(RenderCommand::DrawImage(zpdf_display_list::ImageDraw {
            image_id: page_img_id,
            transform: Matrix::new(10.0, 0.0, 0.0, 10.0, 50.0, 50.0),
            alpha: 1.0,
            is_image_mask: false,
        }));

        let mut diagnostics = zpdf_core::ProcessingDiagnostics::default();
        let images = take_display_list_images(&page_dl, &mut cache, &mut diagnostics);
        assert!(
            diagnostics.resource_limit_reached,
            "exhausting nested tile-replay command budget must report resource_limit_reached"
        );
        assert!(
            images
                .iter()
                .any(|img| img.image.data.first().copied() == Some(42)),
            "later direct page images must still be extracted after tiled replay stops"
        );
    }
}
