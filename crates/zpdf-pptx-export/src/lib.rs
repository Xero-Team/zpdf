//! PDF to PowerPoint (.pptx) export with editable content preservation.
//!
//! This crate converts PDF pages into PowerPoint slides, extracting text with
//! positioning, shapes, and images into native OOXML format rather than simply
//! rasterizing pages to images. The goal is to produce editable presentations.

mod ooxml;
mod shape_recognizer;
mod text_grouper;

use image::ImageEncoder;
use zpdf_core::{Rect, Result};
use zpdf_display_list::{DisplayList, Paint, RenderCommand};
use zpdf_font::FontCache;
use zpdf_image::ImageCache;

pub use ooxml::PptxWriter;

/// A PowerPoint presentation built from PDF pages.
pub struct PptxPresentation {
    pub slides: Vec<PptxSlide>,
    pub width_emu: i64,  // Width in EMUs (English Metric Units, 914400 per inch)
    pub height_emu: i64, // Height in EMUs
}

/// A single PowerPoint slide corresponding to a PDF page.
pub struct PptxSlide {
    pub page_index: usize,
    pub elements: Vec<SlideElement>,
}

/// An element on a PowerPoint slide.
#[derive(Debug, Clone)]
pub enum SlideElement {
    /// Rotation/reflection about an element's center. Existing untransformed
    /// element constructors are unchanged; consumers should handle this variant.
    Transformed {
        element: Box<SlideElement>,
        rotation_degrees: f64,
        flip_h: bool,
        flip_v: bool,
    },
    /// Opacity modifier (`0.0..=1.0`) for a native slide element's solid fill,
    /// stroke, or text paint. Existing opaque element constructors are
    /// unchanged; consumers should handle this variant.
    Opacity {
        element: Box<SlideElement>,
        alpha: f32,
    },
    TextBox {
        text: String,
        x_emu: i64,
        y_emu: i64,
        width_emu: i64,
        height_emu: i64,
        font_family: String,
        font_size_pt: f64,
        color_rgb: (u8, u8, u8),
        bold: bool,
        italic: bool,
    },
    Rectangle {
        x_emu: i64,
        y_emu: i64,
        width_emu: i64,
        height_emu: i64,
        fill_rgb: Option<(u8, u8, u8)>,
        stroke_rgb: Option<(u8, u8, u8)>,
        stroke_width_pt: f64,
    },
    Ellipse {
        x_emu: i64,
        y_emu: i64,
        width_emu: i64,
        height_emu: i64,
        fill_rgb: Option<(u8, u8, u8)>,
        stroke_rgb: Option<(u8, u8, u8)>,
        stroke_width_pt: f64,
    },
    Line {
        x1_emu: i64,
        y1_emu: i64,
        x2_emu: i64,
        y2_emu: i64,
        stroke_rgb: (u8, u8, u8),
        stroke_width_pt: f64,
    },
    FreeformPath {
        path_data: String, // SVG-style path data
        fill_rgb: Option<(u8, u8, u8)>,
        stroke_rgb: Option<(u8, u8, u8)>,
        stroke_width_pt: f64,
    },
    Image {
        x_emu: i64,
        y_emu: i64,
        width_emu: i64,
        height_emu: i64,
        image_data: Vec<u8>, // PNG data
        image_id: String,
    },
}

impl SlideElement {
    pub(crate) fn base(&self) -> &Self {
        let mut element = self;
        loop {
            match element {
                Self::Transformed { element: inner, .. } | Self::Opacity { element: inner, .. } => {
                    element = inner
                }
                _ => return element,
            }
        }
    }
    pub(crate) fn base_mut(&mut self) -> &mut Self {
        let mut element = self;
        loop {
            match element {
                Self::Transformed { element: inner, .. } | Self::Opacity { element: inner, .. } => {
                    element = inner
                }
                _ => return element,
            }
        }
    }
}

fn with_opacity(element: SlideElement, alpha: f32) -> SlideElement {
    let alpha = if alpha.is_finite() {
        alpha.clamp(0.0, 1.0)
    } else {
        0.0
    };
    if (alpha * 100_000.0).round() as u32 >= 100_000 {
        element
    } else {
        SlideElement::Opacity {
            element: Box::new(element),
            alpha,
        }
    }
}

fn transformed_element(element: SlideElement, matrix: &zpdf_core::Matrix) -> SlideElement {
    let rotation_degrees = (-matrix.b.atan2(matrix.a).to_degrees()).rem_euclid(360.0);
    let flip_v = matrix.determinant() < 0.0;
    if rotation_degrees.abs() < 1e-8 && !flip_v {
        element
    } else {
        SlideElement::Transformed {
            element: Box::new(element),
            rotation_degrees,
            flip_h: false,
            flip_v,
        }
    }
}

/// Options for PDF to PowerPoint conversion.
#[derive(Debug, Clone)]
pub struct ConversionOptions {
    /// Minimum text height (in points) to recognize as separate text boxes.
    /// Smaller text might be grouped together.
    pub min_text_box_height: f64,

    /// Maximum horizontal gap (in points) to merge text runs into one box.
    pub text_merge_gap: f64,

    /// Simplify rectangular paths to Rectangle shapes (vs FreeformPath).
    pub recognize_rectangles: bool,

    /// Simplify elliptical paths to Ellipse shapes (vs FreeformPath).
    pub recognize_ellipses: bool,
}

impl Default for ConversionOptions {
    fn default() -> Self {
        Self {
            min_text_box_height: 8.0,
            text_merge_gap: 2.0,
            recognize_rectangles: true,
            recognize_ellipses: true,
        }
    }
}

/// Convert a PDF display list to PowerPoint slide elements.
pub fn display_list_to_slide(
    display_list: &DisplayList,
    page_index: usize,
    font_cache: &FontCache,
    image_cache: &ImageCache,
    options: &ConversionOptions,
) -> Result<PptxSlide> {
    let (slide, _stats) = display_list_to_slide_internal(
        display_list,
        page_index,
        font_cache,
        image_cache,
        options,
        Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
        #[cfg(test)]
        None,
    )?;
    Ok(slide)
}

#[derive(Debug, Default, Clone, Copy)]
struct ConversionStats {
    backdrop_command_visits: usize,
    pass1_command_visits: usize,
    peak_backdrop_pixels: u64,
    peak_rebaser_mask_entries: usize,
    peak_rebaser_mask_bytes: usize,
    peak_flatten_rebaser_mask_entries: usize,
    peak_flatten_rebaser_mask_bytes: usize,
    flatten_cpu_mask_cache_invalidations: usize,
    queued_clip_stack_allocations: usize,
    backdrop_groups_completed: usize,
    native_text_record_visits: usize,
    soft_mask_analysis_visits: usize,
    type3_bounds_cache_entries: usize,
}

fn display_list_to_slide_internal<'a>(
    display_list: &'a DisplayList,
    page_index: usize,
    font_cache: &'a FontCache,
    image_cache: &'a ImageCache,
    options: &'a ConversionOptions,
    render_budget: Option<std::time::Duration>,
    #[cfg(test)] expire_backdrop_after_groups: Option<usize>,
) -> Result<(PptxSlide, ConversionStats)> {
    let mut converter = SlideConverter {
        page_rect: display_list.page_rect,
        page_index,
        font_cache,
        image_cache,
        options,
        elements: Vec::new(),
        removed_element_slots: Vec::new(),
        native_only_texts: NativeOnlyTextIndex::new(display_list.page_rect, false),
        isolated_scope_stack: Vec::new(),
        page_backdrop_index: None,
        soft_mask_cache: SoftMaskAnalysisCache::default(),
        image_counter: 0,
        clip_stack: Vec::new(),
        shared_clip_stack: None,
        backdrop_mgr: BackdropManager::new(render_budget),
        render_budget,
        bounds_deadline: render_budget.map(|b| zpdf_core::time::Instant::now() + b),
        type3_glyph_bounds: std::collections::HashMap::new(),
        stats: ConversionStats::default(),
        #[cfg(test)]
        expire_backdrop_after_groups,
    };

    converter.convert_display_list(display_list)?;
    converter.stats.type3_bounds_cache_entries = converter.type3_glyph_bounds.len();

    Ok((
        PptxSlide {
            page_index,
            elements: converter.elements,
        },
        converter.stats,
    ))
}

const MAX_MASK_RASTER_DIM: f64 = 2048.0;
const MAX_MASK_RASTER_SCALE: f64 = 2.0;
/// Tile size in PDF points for high-resolution (`2.0x`) cropped backdrop tiles
/// on large sheets (`512 × 512` pt -> `1024 × 1024` px = 4 MiB per tile).
const BACKDROP_TILE_PT: f64 = 512.0;
/// Spatial grid cell size in PDF points for indexing native-only (`!has_font_data()`)
/// text records so disjoint labels on dense pages stay O(1) per lookup.
const NATIVE_TEXT_CELL_PT: f64 = 32.0;
/// Total live pixel ceiling across all retained backdrop surfaces (4 Mi pixels = 16 MiB).
const MAX_BACKDROP_CACHE_PIXELS: u64 = 2048 * 2048;

fn fit_scale_to_max_dim(max_dim: f64) -> Option<f32> {
    if !max_dim.is_finite() || max_dim <= 0.0 {
        return None;
    }
    let mut scale = MAX_MASK_RASTER_SCALE.min(MAX_MASK_RASTER_DIM / max_dim) as f32;
    if !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    while (max_dim * scale as f64).ceil() > MAX_MASK_RASTER_DIM && scale > f32::MIN_POSITIVE {
        scale = f32::from_bits(scale.to_bits() - 1);
    }
    Some(scale)
}

struct BackdropSurface<'a> {
    renderer: zpdf_render_cpu::CpuRenderer<'a>,
    font_cache: &'a FontCache,
    rebaser: SoftMaskRebaser,
    rect: Rect,
    scale: f32,
    width: u32,
    height: u32,
    /// Active render time remaining for `execute` calls on this surface
    /// (paused while `SlideConverter` converts other elements or encodes PNGs).
    remaining_budget: Option<std::time::Duration>,
}

impl<'a> BackdropSurface<'a> {
    fn open(
        rect: Rect,
        scale: f32,
        font_cache: &'a FontCache,
        image_cache: &'a ImageCache,
        render_budget: Option<std::time::Duration>,
    ) -> Result<Option<Self>> {
        Self::open_with_background(
            rect,
            scale,
            font_cache,
            image_cache,
            render_budget,
            zpdf_display_list::Color::white(),
        )
    }

    fn open_with_background(
        rect: Rect,
        scale: f32,
        font_cache: &'a FontCache,
        image_cache: &'a ImageCache,
        render_budget: Option<std::time::Duration>,
        background: zpdf_display_list::Color,
    ) -> Result<Option<Self>> {
        use zpdf_render::{PageRenderInfo, RenderBackend};

        let mut renderer = zpdf_render_cpu::CpuRenderer::new()
            .with_fonts(font_cache)
            .with_images(image_cache)
            .with_render_budget(None);
        renderer
            .begin_page(&PageRenderInfo {
                page_rect: rect,
                scale,
                background,
            })
            .map_err(|e| zpdf_core::Error::StreamDecode(format!("PPTX blend render: {e}")))?;
        let Some((width, height, _)) = renderer.current_surface() else {
            return Ok(None);
        };
        let actual_scale = renderer.current_scale();
        let rebaser =
            SoftMaskRebaser::new(rect.x0, rect.y1, 1.0, 1.0, actual_scale).with_target_rect(rect);
        Ok(Some(Self {
            renderer,
            font_cache,
            rebaser,
            rect,
            scale: actual_scale,
            width,
            height,
            remaining_budget: render_budget,
        }))
    }

    fn pixels(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    fn start_batch_deadline(&mut self) -> zpdf_core::time::Instant {
        let now = zpdf_core::time::Instant::now();
        let deadline = self.remaining_budget.map(|rem| now + rem);
        self.renderer.set_deadline(deadline);
        now
    }

    fn finish_batch_deadline(&mut self, batch_start: zpdf_core::time::Instant) {
        if let Some(rem) = &mut self.remaining_budget {
            *rem = rem.saturating_sub(batch_start.elapsed());
        }
        self.renderer.set_deadline(None);
    }

    fn execute_command(&mut self, cmd: &RenderCommand, stats: &mut ConversionStats) -> Result<()> {
        use zpdf_render::RenderBackend;

        // `CpuRenderer` skips outline-less glyph runs without modifying pixels;
        // skipping them here avoids redundant execute calls when advancing an
        // incremental backdrop across many metrics-only labels.
        if let RenderCommand::DrawGlyphRun(run) = cmd {
            if !self
                .font_cache
                .get(run.font_id)
                .is_some_and(|f| f.has_font_data())
            {
                return Ok(());
            }
        }
        let prepared = self.rebaser.prepare_command(cmd);
        let exec_res = self
            .renderer
            .execute(prepared.as_ref())
            .map_err(|e| zpdf_core::Error::StreamDecode(format!("PPTX blend render: {e}")));
        let invalidated = self.rebaser.take_needs_cpu_mask_cache_invalidation();
        if invalidated {
            self.renderer.clear_soft_mask_cache();
        }
        drop(prepared);
        exec_res?;
        stats.backdrop_command_visits += 1;
        stats.peak_rebaser_mask_entries = stats
            .peak_rebaser_mask_entries
            .max(self.rebaser.translated_cache.len());
        stats.peak_rebaser_mask_bytes = stats
            .peak_rebaser_mask_bytes
            .max(self.rebaser.retained_bytes);
        if self.renderer.is_over_budget() {
            return Err(zpdf_core::Error::StreamDecode(
                "PPTX blend backdrop render exceeded time budget".into(),
            ));
        }
        Ok(())
    }
}

struct FullPageBackdrop<'a> {
    surface: BackdropSurface<'a>,
    cursor: usize,
    non_passthrough_stack: Vec<bool>,
}

impl<'a> FullPageBackdrop<'a> {
    fn advance_to(
        &mut self,
        all_commands: &[RenderCommand],
        limit_exclusive: usize,
        stats: &mut ConversionStats,
    ) -> Result<()> {
        if self.cursor >= limit_exclusive {
            return Ok(());
        }
        let batch_start = self.surface.start_batch_deadline();
        let mut batch_err: Option<zpdf_core::Error> = None;
        while self.cursor < limit_exclusive {
            let cmd = &all_commands[self.cursor];
            track_blend_non_passthrough(cmd, &mut self.non_passthrough_stack);
            if let Err(err) = self.surface.execute_command(cmd, stats) {
                batch_err = Some(err);
                break;
            }
            self.cursor += 1;
        }
        self.surface.finish_batch_deadline(batch_start);
        if let Some(err) = batch_err {
            return Err(err);
        }
        Ok(())
    }
}

struct IsolatedScope<'a> {
    body_start_idx: usize,
    pop_idx: usize,
    clip_len_before_push: usize,
    bounds: Rect,
    /// Lazily opened incremental transparent-background backdrop for resolving
    /// multiple overprinting outline-less runs inside this Form in one pass.
    backdrop: Option<FullPageBackdrop<'a>>,
    /// Lazily built spatial index over `all_commands[body_start_idx..pop_idx]`
    /// for Forms exceeding the full-resolution raster cap.
    backdrop_index: Option<BackdropIndex>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TileGroupKey {
    tx_tier: u8,
    gx: i32,
    ty_tier: u8,
    gy: i32,
}

struct PendingTiledSnapshot<'a> {
    element_idx: usize,
    start_idx: usize,
    end_idx: usize,
    clip_stack: std::rc::Rc<[&'a RenderCommand]>,
    crop_rect: Rect,
    /// Native-only (`!has_font_data()`) text bounds overlapped by this snapshot
    /// that could not be promoted cleanly; verified against pass-1 coverage
    /// before failing so non-touching bounding boxes do not false-error.
    unfaithful_native_text_overlaps: Vec<Option<Rect>>,
}

struct NativeOnlyTextRecord {
    element_idx: usize,
    cmd_idx: usize,
    bounds: Option<Rect>,
    resolved_rgb: (u8, u8, u8),
    resolved_alpha: f32,
    is_opaque_black: bool,
    is_opaque_white: bool,
    /// Portion of this run painted over by later intervening paint without
    /// burying the entire run.
    occluded_regions: MissingTextCoverage,
    alive: bool,
    last_stamp: u64,
}

/// Spatially indexed set of emitted outline-less (`!has_font_data()`) native
/// `TextBox` elements on the slide. Disabled completely on ordinary pages with
/// no later overprint or non-Normal blend so disjoint labels pay zero overhead.
struct NativeOnlyTextIndex {
    enabled: bool,
    origin_x: f64,
    origin_y: f64,
    records: Vec<NativeOnlyTextRecord>,
    alive_count: usize,
    large_or_unbounded: Vec<usize>,
    buckets: std::collections::HashMap<(i32, i32), Vec<usize>>,
    visit_stamp: u64,
}

impl NativeOnlyTextIndex {
    fn new(page_rect: Rect, enabled: bool) -> Self {
        Self {
            enabled,
            origin_x: page_rect.x0,
            origin_y: page_rect.y0,
            records: Vec::new(),
            alive_count: 0,
            large_or_unbounded: Vec::new(),
            buckets: std::collections::HashMap::new(),
            visit_stamp: 0,
        }
    }

    fn is_active(&self) -> bool {
        self.enabled && self.alive_count > 0
    }

    fn cell_range(&self, r: Rect) -> Option<(i32, i32, i32, i32)> {
        let r = r.normalize();
        let tx0 = ((r.x0 - self.origin_x) / NATIVE_TEXT_CELL_PT).floor();
        let tx1 = ((r.x1 - self.origin_x) / NATIVE_TEXT_CELL_PT).floor();
        let ty0 = ((r.y0 - self.origin_y) / NATIVE_TEXT_CELL_PT).floor();
        let ty1 = ((r.y1 - self.origin_y) / NATIVE_TEXT_CELL_PT).floor();
        if !(tx0.is_finite() && tx1.is_finite() && ty0.is_finite() && ty1.is_finite()) {
            return None;
        }
        Some((tx0 as i32, tx1 as i32, ty0 as i32, ty1 as i32))
    }

    fn insert(&mut self, rec: NativeOnlyTextRecord) {
        if !self.enabled {
            return;
        }
        const MAX_BUCKET_CELLS: i64 = 16;
        let id = self.records.len();
        let bounds = rec.bounds;
        self.records.push(rec);
        self.alive_count += 1;
        if let Some(r) = bounds {
            if let Some((tx0, tx1, ty0, ty1)) = self.cell_range(r) {
                let cols = tx1 as i64 - tx0 as i64 + 1;
                let rows = ty1 as i64 - ty0 as i64 + 1;
                if cols > 0 && rows > 0 && cols.saturating_mul(rows) <= MAX_BUCKET_CELLS {
                    for ty in ty0..=ty1 {
                        for tx in tx0..=tx1 {
                            self.buckets.entry((tx, ty)).or_default().push(id);
                        }
                    }
                    return;
                }
            }
        }
        self.large_or_unbounded.push(id);
    }

    fn visit_overlapping<F>(&mut self, query: Option<Rect>, stats: &mut ConversionStats, mut f: F)
    where
        F: FnMut(usize, &mut NativeOnlyTextRecord) -> bool,
    {
        if !self.is_active() {
            return;
        }
        const MAX_QUERY_CELLS: i64 = 16;
        self.visit_stamp = self.visit_stamp.wrapping_add(1);
        if self.visit_stamp == 0 {
            for rec in &mut self.records {
                rec.last_stamp = 0;
            }
            self.visit_stamp = 1;
        }
        let stamp = self.visit_stamp;

        let mut retired_any = false;
        let mut handle_id =
            |id: usize, records: &mut [NativeOnlyTextRecord], alive_count: &mut usize| {
                let rec = &mut records[id];
                if !rec.alive || rec.last_stamp == stamp {
                    return;
                }
                rec.last_stamp = stamp;
                stats.native_text_record_visits += 1;
                let overlaps = match (rec.bounds, query) {
                    (Some(tb), Some(qb)) => intersect_rect(tb, qb).is_some(),
                    _ => true,
                };
                if overlaps && !f(id, rec) {
                    rec.alive = false;
                    *alive_count = alive_count.saturating_sub(1);
                    retired_any = true;
                }
            };

        if let Some(q) = query {
            if let Some((tx0, tx1, ty0, ty1)) = self.cell_range(q) {
                let cols = tx1 as i64 - tx0 as i64 + 1;
                let rows = ty1 as i64 - ty0 as i64 + 1;
                if cols > 0 && rows > 0 && cols.saturating_mul(rows) <= MAX_QUERY_CELLS {
                    for &id in &self.large_or_unbounded {
                        handle_id(id, &mut self.records, &mut self.alive_count);
                    }
                    for ty in ty0..=ty1 {
                        for tx in tx0..=tx1 {
                            if let Some(bucket) = self.buckets.get(&(tx, ty)) {
                                for &id in bucket {
                                    handle_id(id, &mut self.records, &mut self.alive_count);
                                }
                            }
                        }
                    }
                    if retired_any && self.alive_count == 0 {
                        self.buckets.clear();
                        self.large_or_unbounded.clear();
                    }
                    return;
                }
            }
        }

        for id in 0..self.records.len() {
            handle_id(id, &mut self.records, &mut self.alive_count);
        }
        if retired_any && self.alive_count == 0 {
            self.buckets.clear();
            self.large_or_unbounded.clear();
        }
    }
}

struct PendingTileGroup<'a> {
    tile_rect: Rect,
    tile_scale: f32,
    snapshots: Vec<PendingTiledSnapshot<'a>>,
}

#[derive(Debug, Clone)]
struct IndexedBackdropItem {
    start: usize,
    end: usize,
    bounds: Option<Rect>,
}

struct BackdropIndex {
    origin_x: f64,
    origin_y: f64,
    items: Vec<IndexedBackdropItem>,
    global_items: Vec<usize>,
    buckets: std::collections::HashMap<(i32, i32), Vec<usize>>,
}

impl BackdropIndex {
    fn cell_range(origin_x: f64, origin_y: f64, r: Rect) -> Option<(i32, i32, i32, i32)> {
        let tx0 = ((r.x0 - origin_x) / BACKDROP_TILE_PT).floor();
        let tx1 = ((r.x1 - origin_x) / BACKDROP_TILE_PT).floor();
        let ty0 = ((r.y0 - origin_y) / BACKDROP_TILE_PT).floor();
        let ty1 = ((r.y1 - origin_y) / BACKDROP_TILE_PT).floor();
        if !(tx0.is_finite() && tx1.is_finite() && ty0.is_finite() && ty1.is_finite()) {
            return None;
        }
        Some((tx0 as i32, tx1 as i32, ty0 as i32, ty1 as i32))
    }

    fn insert_item(&mut self, item: IndexedBackdropItem) {
        const MAX_BUCKET_CELLS: i64 = 16;
        let id = self.items.len();
        let bounds = item.bounds;
        self.items.push(item);
        if let Some(r) = bounds {
            if let Some((tx0, tx1, ty0, ty1)) = Self::cell_range(self.origin_x, self.origin_y, r) {
                let cols = tx1 as i64 - tx0 as i64 + 1;
                let rows = ty1 as i64 - ty0 as i64 + 1;
                if cols > 0 && rows > 0 && cols.saturating_mul(rows) <= MAX_BUCKET_CELLS {
                    for ty in ty0..=ty1 {
                        for tx in tx0..=tx1 {
                            self.buckets.entry((tx, ty)).or_default().push(id);
                        }
                    }
                    return;
                }
            }
        }
        self.global_items.push(id);
    }

    fn build(
        commands: &[RenderCommand],
        page_rect: Rect,
        font_cache: &FontCache,
        soft_mask_cache: &mut SoftMaskAnalysisCache,
        stats: &mut ConversionStats,
    ) -> Self {
        let mut index = Self {
            origin_x: page_rect.x0,
            origin_y: page_rect.y0,
            items: Vec::new(),
            global_items: Vec::new(),
            buckets: std::collections::HashMap::new(),
        };
        // Stack of active open clip bounds: `(pop_idx, clip_bounds_for_bucket, cumulative_bounds)`.
        let mut open_clips: Vec<(usize, Option<Rect>, Rect)> = Vec::new();
        let mut passthrough_blend_depth = 0usize;
        let mut idx = 0usize;

        while idx < commands.len() {
            let active_clip = open_clips.last().map(|&(_, _, c)| c).unwrap_or(page_rect);
            match &commands[idx] {
                RenderCommand::PushClip { .. } | RenderCommand::PushClipStroke { .. } => {
                    if let Some(pop_idx) = SlideConverter::find_matching_pop_clip(commands, idx) {
                        let Some(tightened) = tighten_rect_to_clip(active_clip, &commands[idx])
                        else {
                            idx = pop_idx + 1;
                            continue;
                        };
                        let clip_item_bounds = Some(tightened);
                        index.insert_item(IndexedBackdropItem {
                            start: idx,
                            end: idx,
                            bounds: clip_item_bounds,
                        });
                        open_clips.push((pop_idx, clip_item_bounds, tightened));
                    } else {
                        index.insert_item(IndexedBackdropItem {
                            start: idx,
                            end: idx,
                            bounds: None,
                        });
                    }
                    idx += 1;
                }
                RenderCommand::PopClip => {
                    if open_clips.last().is_some_and(|&(pop, _, _)| pop == idx) {
                        let (_, clip_item_bounds, _) = open_clips.pop().expect("checked");
                        index.insert_item(IndexedBackdropItem {
                            start: idx,
                            end: idx,
                            bounds: clip_item_bounds,
                        });
                    }
                    idx += 1;
                }
                RenderCommand::PushBlendGroup {
                    blend_mode,
                    isolated,
                    knockout,
                    alpha,
                    mask,
                    bounds,
                } => {
                    let group_alpha = if alpha.is_finite() {
                        alpha.clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    let passthrough = !*isolated
                        && !*knockout
                        && *blend_mode == zpdf_display_list::BlendMode::Normal
                        && group_alpha >= 1.0
                        && mask.is_none();
                    if passthrough {
                        passthrough_blend_depth += 1;
                        idx += 1;
                        continue;
                    }
                    if let Some(end_idx) = SlideConverter::find_matching_pop_blend(commands, idx) {
                        if group_alpha <= 0.0 {
                            idx = end_idx + 1;
                            continue;
                        }
                        let group_slice = &commands[idx..=end_idx];
                        let fallback = intersect_rect(*bounds, active_clip).unwrap_or(active_clip);
                        let draw_rect =
                            match compute_commands_bounds(group_slice, (0.0, 0.0), font_cache) {
                                CommandBounds::Empty => {
                                    idx = end_idx + 1;
                                    continue;
                                }
                                CommandBounds::Known(r) => {
                                    Rect::new(r.x0 - 1.0, r.y0 - 1.0, r.x1 + 1.0, r.y1 + 1.0)
                                }
                                CommandBounds::Unknown => fallback,
                            };
                        let masked = match soft_mask_cache
                            .analyze(mask.as_ref(), font_cache, stats)
                            .potential_bounds()
                        {
                            RenderableMaskBounds::Empty => {
                                idx = end_idx + 1;
                                continue;
                            }
                            RenderableMaskBounds::Bounded(padded) => {
                                let Some(both) = intersect_rect(draw_rect, padded) else {
                                    idx = end_idx + 1;
                                    continue;
                                };
                                both
                            }
                            RenderableMaskBounds::Unbounded => draw_rect,
                        };
                        let Some(clipped) = intersect_rect(masked, active_clip) else {
                            idx = end_idx + 1;
                            continue;
                        };
                        index.insert_item(IndexedBackdropItem {
                            start: idx,
                            end: end_idx,
                            bounds: Some(clipped),
                        });
                        idx = end_idx + 1;
                    } else {
                        index.insert_item(IndexedBackdropItem {
                            start: idx,
                            end: idx,
                            bounds: None,
                        });
                        idx += 1;
                    }
                }
                RenderCommand::PopBlendGroup => {
                    if passthrough_blend_depth > 0 {
                        passthrough_blend_depth -= 1;
                    } else {
                        index.insert_item(IndexedBackdropItem {
                            start: idx,
                            end: idx,
                            bounds: None,
                        });
                    }
                    idx += 1;
                }
                _ => {
                    let item_bounds = match compute_commands_bounds(
                        std::slice::from_ref(&commands[idx]),
                        (0.0, 0.0),
                        font_cache,
                    ) {
                        CommandBounds::Empty => {
                            idx += 1;
                            continue;
                        }
                        CommandBounds::Known(r) => {
                            let padded = Rect::new(r.x0 - 1.0, r.y0 - 1.0, r.x1 + 1.0, r.y1 + 1.0);
                            let Some(clipped) = intersect_rect(padded, active_clip) else {
                                idx += 1;
                                continue;
                            };
                            Some(clipped)
                        }
                        CommandBounds::Unknown => Some(active_clip),
                    };
                    index.insert_item(IndexedBackdropItem {
                        start: idx,
                        end: idx,
                        bounds: item_bounds,
                    });
                    idx += 1;
                }
            }
        }
        index
    }

    fn relevant_items_for_rect(&self, rect: Rect) -> Vec<usize> {
        let mut ids: Vec<usize> = Vec::new();
        if let Some((tx0, tx1, ty0, ty1)) = Self::cell_range(self.origin_x, self.origin_y, rect) {
            let cols = tx1 as i64 - tx0 as i64 + 1;
            let rows = ty1 as i64 - ty0 as i64 + 1;
            if cols > 0 && rows > 0 && cols.saturating_mul(rows) <= 16 {
                ids.extend_from_slice(&self.global_items);
                for ty in ty0..=ty1 {
                    for tx in tx0..=tx1 {
                        if let Some(bucket) = self.buckets.get(&(tx, ty)) {
                            ids.extend_from_slice(bucket);
                        }
                    }
                }
                ids.sort_unstable();
                ids.dedup();
                return ids;
            }
        }
        for (id, item) in self.items.iter().enumerate() {
            if item
                .bounds
                .is_none_or(|b| intersect_rect(b, rect).is_some())
            {
                ids.push(id);
            }
        }
        ids
    }
}

struct BackdropManager<'a> {
    full_page: Option<FullPageBackdrop<'a>>,
    pending_groups: Vec<PendingTileGroup<'a>>,
    grid_groups: std::collections::HashMap<TileGroupKey, usize>,
    render_budget: Option<std::time::Duration>,
}

impl<'a> BackdropManager<'a> {
    fn new(render_budget: Option<std::time::Duration>) -> Self {
        Self {
            full_page: None,
            pending_groups: Vec::new(),
            grid_groups: std::collections::HashMap::new(),
            render_budget,
        }
    }

    #[cfg(test)]
    fn expire_all_budgets(&mut self) {
        self.render_budget = Some(std::time::Duration::ZERO);
        if let Some(full) = &mut self.full_page {
            full.surface.remaining_budget = Some(std::time::Duration::ZERO);
        }
    }
}

struct SlideConverter<'a> {
    page_rect: Rect,
    #[allow(dead_code)]
    page_index: usize,
    font_cache: &'a FontCache,
    image_cache: &'a ImageCache,
    options: &'a ConversionOptions,
    elements: Vec<SlideElement>,
    removed_element_slots: Vec<usize>,
    native_only_texts: NativeOnlyTextIndex,
    /// Active unflattened isolated Normal groups with lazy backdrop reuse.
    isolated_scope_stack: Vec<IsolatedScope<'a>>,
    /// Lazily built page-level spatial backdrop index (shared between large-sheet
    /// overprint resolution and deferred tiled snapshot flushes).
    page_backdrop_index: Option<BackdropIndex>,
    /// Shared cache of soft-mask display-list analysis across all mask uses on the slide.
    soft_mask_cache: SoftMaskAnalysisCache,
    image_counter: usize,
    clip_stack: Vec<&'a RenderCommand>,
    shared_clip_stack: Option<std::rc::Rc<[&'a RenderCommand]>>,
    backdrop_mgr: BackdropManager<'a>,
    render_budget: Option<std::time::Duration>,
    /// Shared deadline propagated to Type 3 glyph-bounds measurement.
    bounds_deadline: Option<zpdf_core::time::Instant>,
    /// Per-slide cache of conservative glyph-space ink bounds for synthetic
    /// Type 3 `DrawGlyphRun`s so repeated occurrences never re-interpret
    /// CharProcs or re-decode images.
    type3_glyph_bounds: std::collections::HashMap<(zpdf_font::FontId, u16), Option<Rect>>,
    stats: ConversionStats,
    #[cfg(test)]
    expire_backdrop_after_groups: Option<usize>,
}

#[derive(Debug, Clone, Copy)]
enum ActiveClip {
    None,
    /// Every clip on `clip_stack` is a simple axis-aligned rectangle; `Some` is
    /// their non-empty intersection and `None` means the intersection is empty.
    Rect(Option<Rect>),
    /// At least one clip on `clip_stack` is non-rectangular or a stroke clip.
    Complex,
}

impl<'a> SlideConverter<'a> {
    fn convert_display_list(&mut self, display_list: &'a DisplayList) -> Result<()> {
        let commands = &display_list.commands;
        let expanded = display_list.expanded_glyph_runs();
        self.native_only_texts.enabled = commands.iter().any(|c| {
            matches!(
                c,
                RenderCommand::PushBlendGroup {
                    blend_mode,
                    alpha,
                    ..
                } if *blend_mode != zpdf_display_list::BlendMode::Normal
                    && alpha.is_finite()
                    && *alpha > 0.0
            ) || command_has_renderable_overprint(c, self.font_cache)
                || command_has_backdrop_dependent_native_overprint(c, self.font_cache)
                || command_uses_tiling_backdrop(c, self.font_cache)
        });
        let isolated_inner_blends = compute_isolated_inner_non_normal_blends(
            commands,
            self.font_cache,
            &mut self.soft_mask_cache,
            &mut self.stats,
        );
        let later_non_normal_blends = NonNormalBlendIndex::build(
            commands,
            self.page_rect,
            self.font_cache,
            &mut self.soft_mask_cache,
            &mut self.stats,
            !expanded.is_empty(),
        );
        let mut exp_idx = 0usize;
        let mut cmd_idx = 0usize;
        let mut fallback_artwork_end = 0usize;

        while cmd_idx < commands.len() || exp_idx < expanded.len() {
            let mut skipped_to: Option<usize> = None;
            while exp_idx < expanded.len() && expanded[exp_idx].cmd_start <= cmd_idx {
                let entry = &expanded[exp_idx];
                exp_idx += 1;
                if !display_list.is_in_pattern_range(entry.cmd_start) {
                    let effective_alpha = effective_paint_alpha(&entry.run.paint, entry.run.alpha);
                    if effective_alpha <= 0.0 {
                        let end = entry.cmd_end.min(commands.len());
                        if end > cmd_idx {
                            skipped_to = Some(skipped_to.map_or(end, |prev| prev.max(end)));
                        }
                    } else {
                        let end = entry.cmd_end.min(commands.len());
                        let range_has_overprint = entry.run.overprint.is_some()
                            || commands[entry.cmd_start.min(end)..end]
                                .iter()
                                .any(command_has_visible_overprint);
                        let run_bounds = expanded_run_bounds(entry, commands, self.font_cache);
                        let overlapped_by_later_blend =
                            later_non_normal_blends.overlaps_at_or_after(entry.cmd_end, run_bounds);
                        if effective_alpha >= 1.0 - 1e-3
                            && !range_has_overprint
                            && !overlapped_by_later_blend
                            && !self.clip_stack_constrains_rect(run_bounds)
                            && self.convert_glyph_run(&entry.run, true)?
                        {
                            self.note_paint_over_native_texts(
                                run_bounds,
                                None,
                                glyph_run_is_opaque_black(&entry.run),
                                glyph_run_is_opaque_white(&entry.run),
                            );
                            if end > cmd_idx {
                                skipped_to = Some(skipped_to.map_or(end, |prev| prev.max(end)));
                            }
                        } else {
                            fallback_artwork_end = fallback_artwork_end.max(end);
                        }
                    }
                }
            }
            if let Some(end) = skipped_to {
                for (offset, cmd) in commands[cmd_idx..end].iter().enumerate() {
                    self.track_clip(cmd);
                    let idx = cmd_idx + offset;
                    if matches!(cmd, RenderCommand::PopBlendGroup)
                        && self
                            .isolated_scope_stack
                            .last()
                            .is_some_and(|scope| scope.pop_idx == idx)
                    {
                        self.isolated_scope_stack.pop();
                    }
                }
                cmd_idx = end;
                continue;
            }
            if cmd_idx >= commands.len() {
                break;
            }
            if let RenderCommand::PushBlendGroup {
                blend_mode,
                isolated,
                knockout,
                alpha,
                mask,
                bounds,
            } = &commands[cmd_idx]
            {
                let group_alpha = if alpha.is_finite() {
                    alpha.clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let is_non_normal_blend = *blend_mode != zpdf_display_list::BlendMode::Normal;
                let is_translucent_group = group_alpha < 1.0;
                let isolated_with_inner_blend = *isolated && isolated_inner_blends[cmd_idx];
                let needs_group_flatten = mask.is_some()
                    || is_non_normal_blend
                    || is_translucent_group
                    || *knockout
                    || isolated_with_inner_blend;
                if needs_group_flatten {
                    if let Some(end_idx) = Self::find_matching_pop_blend(commands, cmd_idx) {
                        if group_alpha <= 0.0 {
                            // Zero-opacity transparency group hides everything inside
                            // without inspecting or rasterizing its soft mask.
                            for cmd in &commands[cmd_idx..=end_idx] {
                                self.track_clip(cmd);
                            }
                            while exp_idx < expanded.len() && expanded[exp_idx].cmd_start <= end_idx
                            {
                                exp_idx += 1;
                            }
                            cmd_idx = end_idx + 1;
                            continue;
                        }
                        let mask_analysis = self.soft_mask_cache.analyze(
                            mask.as_ref(),
                            self.font_cache,
                            &mut self.stats,
                        );
                        if mask_analysis.is_proven_empty() {
                            // Empty `/S /Alpha` soft mask (with no transfer function
                            // and no outline-less mask glyphs) hides everything inside.
                            for cmd in &commands[cmd_idx..=end_idx] {
                                self.track_clip(cmd);
                            }
                            while exp_idx < expanded.len() && expanded[exp_idx].cmd_start <= end_idx
                            {
                                exp_idx += 1;
                            }
                            cmd_idx = end_idx + 1;
                            continue;
                        }
                        let group_slice = &commands[cmd_idx..=end_idx];
                        if group_overlaps_missing_mask_text(
                            &mask_analysis.missing_text,
                            group_slice,
                            &self.clip_stack,
                            self.page_rect,
                            self.font_cache,
                            &mut self.soft_mask_cache,
                            &mut self.stats,
                        ) {
                            return Err(zpdf_core::Error::StreamDecode(
                                "PPTX soft mask contains text without font outlines and cannot be rendered faithfully".into(),
                            ));
                        }
                        if matches!(mask_analysis.renderable, RenderableMaskBounds::Empty) {
                            // Missing mask glyphs did not overlap any visible paint
                            // in this group, and the mask has no other renderable ink.
                            for cmd in group_slice {
                                self.track_clip(cmd);
                            }
                            while exp_idx < expanded.len() && expanded[exp_idx].cmd_start <= end_idx
                            {
                                exp_idx += 1;
                            }
                            cmd_idx = end_idx + 1;
                            continue;
                        }
                        // Soft masks, group opacity, knockout compositing, non-Normal
                        // blend modes, and isolated groups with inner non-Normal blends
                        // apply to the entire group even when it contains editable text.
                        let is_opaque_isolated_normal = *isolated
                            && !*knockout
                            && !is_non_normal_blend
                            && !is_translucent_group
                            && mask.is_none();
                        let surviving_native_texts = self.plan_flattened_group_native_texts(
                            display_list,
                            cmd_idx,
                            group_slice,
                            *bounds,
                            is_opaque_isolated_normal,
                            mask_analysis.renderable,
                        )?;
                        let uses_backdrop = group_uses_page_backdrop(group_slice, self.font_cache);
                        let backdrop =
                            uses_backdrop.then_some((commands.as_slice(), cmd_idx, end_idx));
                        let group_clip_len = self.clip_stack.len();
                        self.flatten_commands_group(group_slice, mask.as_ref(), *bounds, backdrop)?;
                        if !surviving_native_texts.is_empty() {
                            let base_scope_depth = self.isolated_scope_stack.len();
                            let mut emit_err = None;
                            for planned in surviving_native_texts {
                                // Keep the longest matching ancestor prefix of open
                                // promoted scopes alive so leaving a child Form resumes
                                // the parent's cached FullPageBackdrop and cursor.
                                let open_promoted = &self.isolated_scope_stack[base_scope_depth..];
                                let mut common = 0usize;
                                while common < open_promoted.len()
                                    && common < planned.scope_chain.len()
                                    && open_promoted[common].body_start_idx
                                        == planned.scope_chain[common].body_start
                                    && open_promoted[common].pop_idx
                                        == planned.scope_chain[common].pop_idx
                                {
                                    common += 1;
                                }
                                let target_depth = base_scope_depth + common;
                                if self.isolated_scope_stack.len() > target_depth {
                                    self.isolated_scope_stack.truncate(target_depth);
                                    let restored_clip_len = if common > 0 {
                                        self.isolated_scope_stack[target_depth - 1]
                                            .clip_len_before_push
                                    } else {
                                        group_clip_len
                                    };
                                    self.clip_stack.truncate(restored_clip_len);
                                    self.shared_clip_stack = None;
                                }
                                let mut active_extra_len =
                                    self.clip_stack.len().saturating_sub(group_clip_len);
                                for scope in &planned.scope_chain[common..] {
                                    let start_clip = active_extra_len.min(scope.extra_clips.len());
                                    for &clip_abs in &scope.extra_clips[start_clip..] {
                                        self.clip_stack.push(&commands[clip_abs]);
                                    }
                                    active_extra_len = scope.extra_clips.len();
                                    self.shared_clip_stack = None;
                                    let clip_len_before = self.clip_stack.len();
                                    self.isolated_scope_stack.push(IsolatedScope {
                                        body_start_idx: scope.body_start,
                                        pop_idx: scope.pop_idx,
                                        clip_len_before_push: clip_len_before,
                                        bounds: scope.bounds,
                                        backdrop: None,
                                        backdrop_index: None,
                                    });
                                }
                                if let RenderCommand::DrawGlyphRun(run) =
                                    &group_slice[planned.rel_idx]
                                {
                                    if let Err(err) = self.emit_native_only_glyph_run(
                                        commands,
                                        cmd_idx + planned.rel_idx,
                                        run,
                                    ) {
                                        emit_err = Some(err);
                                        break;
                                    }
                                }
                            }
                            self.isolated_scope_stack.truncate(base_scope_depth);
                            self.clip_stack.truncate(group_clip_len);
                            self.shared_clip_stack = None;
                            if let Some(err) = emit_err {
                                return Err(err);
                            }
                        }
                        for cmd in group_slice {
                            self.track_clip(cmd);
                        }
                        while exp_idx < expanded.len() && expanded[exp_idx].cmd_start <= end_idx {
                            exp_idx += 1;
                        }
                        cmd_idx = end_idx + 1;
                        continue;
                    }
                } else if *isolated {
                    if let Some(end_idx) = Self::find_matching_pop_blend(commands, cmd_idx) {
                        self.isolated_scope_stack.push(IsolatedScope {
                            body_start_idx: cmd_idx + 1,
                            pop_idx: end_idx,
                            clip_len_before_push: self.clip_stack.len(),
                            bounds: *bounds,
                            backdrop: None,
                            backdrop_index: None,
                        });
                    }
                }
            } else if matches!(commands[cmd_idx], RenderCommand::PopBlendGroup)
                && self
                    .isolated_scope_stack
                    .last()
                    .is_some_and(|scope| scope.pop_idx == cmd_idx)
            {
                self.isolated_scope_stack.pop();
            }
            if !self.clip_stack.is_empty() {
                if let Some(g_end) = type3_glyph_range_starting_at(display_list, cmd_idx) {
                    let g_end = g_end.min(commands.len());
                    if g_end > cmd_idx
                        && self.clip_stack_constrains_commands(&commands[cmd_idx..g_end])
                    {
                        let group_slice = &commands[cmd_idx..g_end];
                        self.plan_flattened_group_native_texts(
                            display_list,
                            cmd_idx,
                            group_slice,
                            self.page_rect,
                            false,
                            RenderableMaskBounds::Unbounded,
                        )?;
                        let uses_backdrop = group_uses_page_backdrop(group_slice, self.font_cache);
                        let backdrop =
                            uses_backdrop.then_some((commands.as_slice(), cmd_idx, g_end - 1));
                        self.flatten_commands_group(group_slice, None, self.page_rect, backdrop)?;
                        for cmd in group_slice {
                            self.track_clip(cmd);
                        }
                        while exp_idx < expanded.len() && expanded[exp_idx].cmd_start < g_end {
                            exp_idx += 1;
                        }
                        cmd_idx = g_end;
                        continue;
                    }
                }
            }
            if matches!(
                commands[cmd_idx],
                RenderCommand::PushClip { .. } | RenderCommand::PushClipStroke { .. }
            ) && display_list.is_in_type3_glyph_range(cmd_idx)
            {
                if let Some(end_idx) = Self::find_matching_pop_clip(commands, cmd_idx) {
                    let group_slice = &commands[cmd_idx..=end_idx];
                    self.plan_flattened_group_native_texts(
                        display_list,
                        cmd_idx,
                        group_slice,
                        self.page_rect,
                        false,
                        RenderableMaskBounds::Unbounded,
                    )?;
                    let uses_backdrop = group_uses_page_backdrop(group_slice, self.font_cache);
                    let backdrop = uses_backdrop.then_some((commands.as_slice(), cmd_idx, end_idx));
                    self.flatten_commands_group(group_slice, None, self.page_rect, backdrop)?;
                    for cmd in group_slice {
                        self.track_clip(cmd);
                    }
                    while exp_idx < expanded.len() && expanded[exp_idx].cmd_start <= end_idx {
                        exp_idx += 1;
                    }
                    cmd_idx = end_idx + 1;
                    continue;
                }
            }
            self.track_clip(&commands[cmd_idx]);
            let in_artwork_range = cmd_idx < fallback_artwork_end
                || display_list.is_in_type3_glyph_range(cmd_idx)
                || display_list.is_in_pattern_range(cmd_idx);
            self.convert_command(commands, cmd_idx, in_artwork_range)?;
            cmd_idx += 1;
        }
        self.flush_pending_tiled_snapshots(commands)?;
        self.compact_removed_elements();
        Ok(())
    }

    fn clip_stack_constrains_rect(&self, bounds: Option<Rect>) -> bool {
        clip_stack_constrains_rect_for(&self.clip_stack, bounds)
    }

    fn clipped_command_bounds(&self, cmd: &RenderCommand) -> CommandBounds {
        let raw = compute_commands_bounds(std::slice::from_ref(cmd), (0.0, 0.0), self.font_cache);
        let CommandBounds::Known(mut r) = raw else {
            return raw;
        };
        for &clip in &self.clip_stack {
            let Some(t) = tighten_rect_to_clip(r, clip) else {
                return CommandBounds::Empty;
            };
            r = t;
        }
        CommandBounds::Known(r)
    }

    fn type3_glyph_local_bounds(
        &mut self,
        font_id: zpdf_font::FontId,
        font: &zpdf_font::LoadedFont,
        glyph_id: u16,
    ) -> Result<Option<Rect>> {
        const MAX_CACHED_TYPE3_BOUNDS: usize = 4096;
        if let Some(&cached) = self.type3_glyph_bounds.get(&(font_id, glyph_id)) {
            return Ok(cached);
        }
        if self
            .bounds_deadline
            .is_some_and(|d| zpdf_core::time::Instant::now() >= d)
        {
            return Err(zpdf_core::Error::StreamDecode(
                "PPTX Type 3 bounds measurement exceeded time budget".into(),
            ));
        }
        let Some((stream, _)) = font.type3_glyph_stream(glyph_id) else {
            return Ok(None);
        };
        let glyph_rect = Rect::new(0.0, -1000.0, 1000.0, 1000.0);
        let mut images = ImageCache::new();
        let mut interp = zpdf_content::interpreter::ContentInterpreter::new(glyph_rect)
            .for_type3_glyph()
            .with_type3_resources(font.type3_resources())
            .with_images(&mut images);
        if let Some(d) = self.bounds_deadline {
            interp = interp.with_deadline(d);
        }
        let (dl, stats) = interp.interpret_with_stats(stream);
        if stats.diagnostics.time_limit_reached {
            return Err(zpdf_core::Error::StreamDecode(
                "PPTX Type 3 bounds measurement exceeded time budget".into(),
            ));
        }

        #[derive(Clone, Copy)]
        enum ClipBound {
            Unbounded,
            Empty,
            Rect(Rect),
        }

        let local_stroke_overshoot = |style: &zpdf_display_list::StrokeStyle| -> f64 {
            let width = if style.width.is_finite() {
                style.width.max(0.0) as f64
            } else {
                0.0
            };
            let reach = match style.join {
                zpdf_display_list::LineJoin::Miter => style.miter_limit.clamp(1.0, 100.0) as f64,
                _ => std::f64::consts::SQRT_2,
            };
            (width * 0.5 * reach.max(std::f64::consts::SQRT_2)).max(1e-3)
        };
        let pad_rect = |r: Rect, pad: f64| -> Option<Rect> {
            let r = r.normalize();
            (r.x0.is_finite()
                && r.y0.is_finite()
                && r.x1.is_finite()
                && r.y1.is_finite()
                && pad.is_finite()
                && pad >= 0.0)
                .then(|| Rect::new(r.x0 - pad, r.y0 - pad, r.x1 + pad, r.y1 + pad))
        };
        let union_rect = |acc: Option<Rect>, r: Rect| -> Option<Rect> {
            let r = r.normalize();
            if !(r.x0.is_finite() && r.y0.is_finite() && r.x1.is_finite() && r.y1.is_finite()) {
                return acc;
            }
            Some(match acc {
                Some(a) => Rect::new(
                    a.x0.min(r.x0),
                    a.y0.min(r.y0),
                    a.x1.max(r.x1),
                    a.y1.max(r.y1),
                ),
                None => r,
            })
        };

        let mut acc: Option<Rect> = None;
        let mut clips: Vec<ClipBound> = Vec::new();
        let mut push_rect = |r: Rect, clips: &[ClipBound]| {
            let effective = match clips.last().copied().unwrap_or(ClipBound::Unbounded) {
                ClipBound::Unbounded => Some(r),
                ClipBound::Empty => None,
                ClipBound::Rect(cb) => intersect_rect(r, cb),
            };
            if let Some(clipped) = effective {
                acc = union_rect(acc, clipped);
            }
        };

        for cmd in &dl.commands {
            match cmd {
                RenderCommand::PushClip { path, .. } => {
                    let top = clips.last().copied().unwrap_or(ClipBound::Unbounded);
                    let next = if path.is_empty() {
                        ClipBound::Empty
                    } else if let Some(cb) = path_control_bounds(path) {
                        match top {
                            ClipBound::Unbounded => ClipBound::Rect(cb),
                            ClipBound::Empty => ClipBound::Empty,
                            ClipBound::Rect(prev) => {
                                intersect_rect(prev, cb).map_or(ClipBound::Empty, ClipBound::Rect)
                            }
                        }
                    } else {
                        top
                    };
                    clips.push(next);
                }
                RenderCommand::PushClipStroke { path, style } => {
                    let top = clips.last().copied().unwrap_or(ClipBound::Unbounded);
                    let pad = local_stroke_overshoot(style);
                    let next = if path.is_empty() {
                        ClipBound::Empty
                    } else if let Some(cb) =
                        path_control_bounds(path).and_then(|r| pad_rect(r, pad))
                    {
                        match top {
                            ClipBound::Unbounded => ClipBound::Rect(cb),
                            ClipBound::Empty => ClipBound::Empty,
                            ClipBound::Rect(prev) => {
                                intersect_rect(prev, cb).map_or(ClipBound::Empty, ClipBound::Rect)
                            }
                        }
                    } else {
                        top
                    };
                    clips.push(next);
                }
                RenderCommand::PopClip => {
                    clips.pop();
                }
                RenderCommand::FillPath { path, .. } => {
                    if let Some(r) = path_control_bounds(path) {
                        push_rect(r, &clips);
                    }
                }
                RenderCommand::StrokePath { path, style, .. } => {
                    let pad = local_stroke_overshoot(style);
                    if let Some(r) = path_control_bounds(path).and_then(|b| pad_rect(b, pad)) {
                        push_rect(r, &clips);
                    }
                }
                RenderCommand::DrawImage(draw) if draw.transform.is_finite() => {
                    let corners = [
                        zpdf_core::Point::new(0.0, 0.0).transform(&draw.transform),
                        zpdf_core::Point::new(1.0, 0.0).transform(&draw.transform),
                        zpdf_core::Point::new(0.0, 1.0).transform(&draw.transform),
                        zpdf_core::Point::new(1.0, 1.0).transform(&draw.transform),
                    ];
                    let (mut x0, mut y0) = (f64::INFINITY, f64::INFINITY);
                    let (mut x1, mut y1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
                    for p in corners {
                        x0 = x0.min(p.x);
                        y0 = y0.min(p.y);
                        x1 = x1.max(p.x);
                        y1 = y1.max(p.y);
                    }
                    if x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite() {
                        push_rect(Rect::new(x0, y0, x1, y1), &clips);
                    }
                }
                RenderCommand::DrawInlineImage(draw) if draw.transform.is_finite() => {
                    let corners = [
                        zpdf_core::Point::new(0.0, 0.0).transform(&draw.transform),
                        zpdf_core::Point::new(1.0, 0.0).transform(&draw.transform),
                        zpdf_core::Point::new(0.0, 1.0).transform(&draw.transform),
                        zpdf_core::Point::new(1.0, 1.0).transform(&draw.transform),
                    ];
                    let (mut x0, mut y0) = (f64::INFINITY, f64::INFINITY);
                    let (mut x1, mut y1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
                    for p in corners {
                        x0 = x0.min(p.x);
                        y0 = y0.min(p.y);
                        x1 = x1.max(p.x);
                        y1 = y1.max(p.y);
                    }
                    if x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite() {
                        push_rect(Rect::new(x0, y0, x1, y1), &clips);
                    }
                }
                _ => {}
            }
        }
        if self.type3_glyph_bounds.len() < MAX_CACHED_TYPE3_BOUNDS {
            self.type3_glyph_bounds.insert((font_id, glyph_id), acc);
        }
        Ok(acc)
    }

    fn type3_glyph_run_bounds(
        &mut self,
        run: &zpdf_display_list::GlyphRun,
    ) -> Result<CommandBounds> {
        if run.glyphs.is_empty() {
            return Ok(CommandBounds::Empty);
        }
        if !run.transform.is_finite() || !run.font_size.is_finite() || !run.h_scale.is_finite() {
            return Ok(CommandBounds::Unknown);
        }
        let Some(font) = self.font_cache.get(run.font_id) else {
            return Ok(CommandBounds::Unknown);
        };
        let (mut x0, mut y0) = (f64::INFINITY, f64::INFINITY);
        let (mut x1, mut y1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        let mut saw_paint = false;
        for (i, glyph) in run.glyphs.iter().enumerate() {
            if (i & 63) == 0
                && self
                    .bounds_deadline
                    .is_some_and(|d| zpdf_core::time::Instant::now() >= d)
            {
                return Err(zpdf_core::Error::StreamDecode(
                    "PPTX Type 3 bounds measurement exceeded time budget".into(),
                ));
            }
            let Some((_, fm)) = font.type3_glyph_stream(glyph.glyph_id) else {
                continue;
            };
            let Some(local) = self.type3_glyph_local_bounds(run.font_id, font, glyph.glyph_id)?
            else {
                continue;
            };
            let glyph_to_page = run
                .transform
                .concat(&zpdf_core::Matrix::translate(
                    glyph.x as f64,
                    glyph.y as f64,
                ))
                .concat(&zpdf_core::Matrix::scale(
                    run.font_size as f64 * run.h_scale as f64,
                    run.font_size as f64,
                ))
                .concat(&zpdf_core::Matrix::new(
                    fm[0], fm[1], fm[2], fm[3], fm[4], fm[5],
                ));
            if !glyph_to_page.is_finite() {
                return Ok(CommandBounds::Unknown);
            }
            for p in [
                zpdf_core::Point::new(local.x0, local.y0).transform(&glyph_to_page),
                zpdf_core::Point::new(local.x1, local.y0).transform(&glyph_to_page),
                zpdf_core::Point::new(local.x0, local.y1).transform(&glyph_to_page),
                zpdf_core::Point::new(local.x1, local.y1).transform(&glyph_to_page),
            ] {
                if !p.x.is_finite() || !p.y.is_finite() {
                    return Ok(CommandBounds::Unknown);
                }
                x0 = x0.min(p.x);
                y0 = y0.min(p.y);
                x1 = x1.max(p.x);
                y1 = y1.max(p.y);
                saw_paint = true;
            }
        }
        if !saw_paint {
            return Ok(CommandBounds::Empty);
        }
        Ok(CommandBounds::Known(Rect::new(x0, y0, x1, y1)))
    }

    fn compute_commands_bounds_cached(
        &mut self,
        commands: &[RenderCommand],
    ) -> Result<CommandBounds> {
        let has_type3 = commands.iter().any(|cmd| {
            matches!(
                cmd,
                RenderCommand::DrawGlyphRun(run)
                    if !run.glyphs.is_empty()
                        && self.font_cache.get(run.font_id).is_some_and(|f| f.is_type3())
            )
        });
        if !has_type3 {
            return Ok(compute_commands_bounds(
                commands,
                (0.0, 0.0),
                self.font_cache,
            ));
        }
        let mut acc: Option<Rect> = None;
        for cmd in commands {
            let b = match cmd {
                RenderCommand::DrawGlyphRun(run)
                    if self
                        .font_cache
                        .get(run.font_id)
                        .is_some_and(|f| f.is_type3()) =>
                {
                    self.type3_glyph_run_bounds(run)?
                }
                other => compute_commands_bounds(
                    std::slice::from_ref(other),
                    (0.0, 0.0),
                    self.font_cache,
                ),
            };
            match b {
                CommandBounds::Empty => {}
                CommandBounds::Unknown => return Ok(CommandBounds::Unknown),
                CommandBounds::Known(r) => {
                    acc = Some(match acc {
                        Some(a) => Rect::new(
                            a.x0.min(r.x0),
                            a.y0.min(r.y0),
                            a.x1.max(r.x1),
                            a.y1.max(r.y1),
                        ),
                        None => r,
                    });
                }
            }
        }
        Ok(acc.map_or(CommandBounds::Empty, CommandBounds::Known))
    }

    fn live_backdrop_pixels(&self) -> u64 {
        let full = self
            .backdrop_mgr
            .full_page
            .as_ref()
            .map_or(0, |f| f.surface.pixels());
        let scopes: u64 = self
            .isolated_scope_stack
            .iter()
            .filter_map(|s| s.backdrop.as_ref())
            .map(|b| b.surface.pixels())
            .sum();
        full.saturating_add(scopes)
    }

    /// Resolve the final `(r, g, b)` color and opacity of an overprinting
    /// outline-less (`!has_font_data()`) glyph run against the active isolation
    /// scope's backdrop (opaque paper white at page level, transparent inside an
    /// isolated Normal Form).
    ///
    /// Reuses an incremental backdrop surface within standard-sized pages/Forms
    /// or a lazily built spatial `BackdropIndex` on large sheets/Forms so
    /// multiple labels scale linearly instead of replaying all prior commands.
    fn resolve_native_only_overprint_paint(
        &mut self,
        all_commands: &[RenderCommand],
        cmd_idx: usize,
        glyph_run: &zpdf_display_list::GlyphRun,
        op: zpdf_display_list::Overprint,
    ) -> Result<((u8, u8, u8), f32)> {
        use zpdf_render::{PageRenderInfo, RenderBackend};

        let effective_alpha = effective_paint_alpha(&glyph_run.paint, glyph_run.alpha);
        // Backdrop-independent fast path: opaque K=1 (or all 4 process colorants)
        // resolves identically to opaque color on every possible backdrop (including
        // transparent isolated Forms where `out_a = ba + 1 * (1 - ba) = 1`).
        if effective_alpha >= 1.0 - 1e-3
            && ((op.paints(3) && op.cmyk[3] >= 1.0 - 1e-3) || op.active == 0b1111)
        {
            let rgb = resolve_overprint_color_on_rgb((255, 255, 255), &op, 1.0);
            return Ok((rgb, 1.0));
        }

        let in_isolated_scope = !self.isolated_scope_stack.is_empty();
        let (backdrop_start_idx, scope_end_idx, initial_clip_len, scope_bounds) =
            if let Some(scope) = self.isolated_scope_stack.last() {
                (
                    scope.body_start_idx,
                    scope.pop_idx,
                    scope.clip_len_before_push,
                    scope.bounds,
                )
            } else {
                (0, all_commands.len(), 0, self.page_rect)
            };
        let empty_bg_rgba = if in_isolated_scope {
            [0, 0, 0, 0]
        } else {
            [255, 255, 255, 255]
        };

        let Some(bounds) = native_text_ink_bounds(glyph_run, self.font_cache) else {
            return Err(zpdf_core::Error::StreamDecode(
                "PPTX overprinting native-only text has non-finite bounds and cannot resolve backdrop color".into(),
            ));
        };
        let Some(target_rect) = intersect_rect(bounds, self.page_rect)
            .and_then(|r| tighten_rect_to_clips(r, &self.clip_stack))
        else {
            // Fully clipped or off-page (caller skips emitting invisible runs).
            return Ok(((0, 0, 0), 0.0));
        };

        let sample = if cmd_idx <= backdrop_start_idx {
            let (rgb, alpha) =
                resolve_overprint_on_premul_rgba(empty_bg_rgba, &op, effective_alpha)?;
            ResolvedOverprintSample {
                rgb,
                alpha,
                uniform_bg_px: Some(empty_bg_rgba),
            }
        } else {
            // Try reusing an incremental full-resolution (2.0x) backdrop surface
            // for the active scope so N labels over a shared backdrop execute
            // preceding commands at most once in total.
            let mut sampled: Option<ResolvedOverprintSample> = None;
            if in_isolated_scope {
                let scope_rect = intersect_rect(scope_bounds, self.page_rect)
                    .filter(|r| rect_contains(*r, target_rect))
                    .unwrap_or(self.page_rect);
                let scope_max_dim = scope_rect.width().max(scope_rect.height());
                let scope_scale = fit_scale_to_max_dim(scope_max_dim);
                let full_res =
                    scope_scale.is_some_and(|s| s >= MAX_MASK_RASTER_SCALE as f32 - 1e-6);
                if full_res {
                    let scale = scope_scale.expect("checked");
                    let need_open = self
                        .isolated_scope_stack
                        .last()
                        .is_some_and(|s| s.backdrop.is_none());
                    let est_w = (scope_rect.width() * scale as f64).ceil().max(1.0) as u64;
                    let est_h = (scope_rect.height() * scale as f64).ceil().max(1.0) as u64;
                    let est_px = est_w.saturating_mul(est_h);
                    let fits_budget = !need_open
                        || self.live_backdrop_pixels().saturating_add(est_px)
                            <= MAX_BACKDROP_CACHE_PIXELS;
                    if fits_budget {
                        if need_open {
                            if let Some(mut surface) = BackdropSurface::open_with_background(
                                scope_rect,
                                scale,
                                self.font_cache,
                                self.image_cache,
                                self.backdrop_mgr.render_budget,
                                zpdf_display_list::Color::rgba(0.0, 0.0, 0.0, 0.0),
                            )? {
                                let clip_prefix_len = initial_clip_len.min(self.clip_stack.len());
                                let batch_start = surface.start_batch_deadline();
                                let mut clip_err = None;
                                for &clip in &self.clip_stack[..clip_prefix_len] {
                                    if let Err(err) = surface.execute_command(clip, &mut self.stats)
                                    {
                                        clip_err = Some(err);
                                        break;
                                    }
                                }
                                surface.finish_batch_deadline(batch_start);
                                if let Some(err) = clip_err {
                                    return Err(err);
                                }
                                let scope = self.isolated_scope_stack.last_mut().expect("in scope");
                                scope.backdrop = Some(FullPageBackdrop {
                                    surface,
                                    cursor: backdrop_start_idx,
                                    non_passthrough_stack: Vec::new(),
                                });
                                let live = self.live_backdrop_pixels();
                                self.stats.peak_backdrop_pixels =
                                    self.stats.peak_backdrop_pixels.max(live);
                            }
                        }
                        if let Some(full) = self
                            .isolated_scope_stack
                            .last_mut()
                            .and_then(|s| s.backdrop.as_mut())
                        {
                            if rect_contains(full.surface.rect, target_rect) {
                                full.advance_to(all_commands, cmd_idx, &mut self.stats)?;
                                if full.non_passthrough_stack.iter().any(|&np| np) {
                                    return Err(zpdf_core::Error::StreamDecode(
                                        "PPTX overprinting native-only text sits inside an unclosed non-passthrough group".into(),
                                    ));
                                }
                                let surf =
                                    full.surface.renderer.current_surface().ok_or_else(|| {
                                        zpdf_core::Error::StreamDecode(
                                            "PPTX overprint backdrop surface unavailable".into(),
                                        )
                                    })?;
                                sampled = Some(resolve_uniform_overprint_in_surface(
                                    surf,
                                    full.surface.rect,
                                    full.surface.scale,
                                    target_rect,
                                    &op,
                                    effective_alpha,
                                    empty_bg_rgba,
                                )?);
                            }
                        }
                    }
                }
            } else {
                let page_max_dim = self.page_rect.width().max(self.page_rect.height());
                if let Some(page_scale) = fit_scale_to_max_dim(page_max_dim) {
                    if page_scale >= MAX_MASK_RASTER_SCALE as f32 - 1e-6
                        && self.ensure_full_page_backdrop(page_scale)?.is_some()
                    {
                        let full = self.backdrop_mgr.full_page.as_mut().expect("initialized");
                        full.advance_to(all_commands, cmd_idx, &mut self.stats)?;
                        if full.non_passthrough_stack.iter().any(|&np| np) {
                            return Err(zpdf_core::Error::StreamDecode(
                                "PPTX overprinting native-only text sits inside an unclosed non-passthrough group".into(),
                            ));
                        }
                        let surf = full.surface.renderer.current_surface().ok_or_else(|| {
                            zpdf_core::Error::StreamDecode(
                                "PPTX overprint backdrop surface unavailable".into(),
                            )
                        })?;
                        sampled = Some(resolve_uniform_overprint_in_surface(
                            surf,
                            full.surface.rect,
                            full.surface.scale,
                            target_rect,
                            &op,
                            effective_alpha,
                            empty_bg_rgba,
                        )?);
                    }
                }
            }

            if let Some(res) = sampled {
                res
            } else {
                // Large sheet/Form fallback: lazily build a spatial `BackdropIndex`
                // once per scope and replay ONLY the items overlapping `target_rect`
                // before `cmd_idx`, keeping per-label work proportional to local
                // overlapping geometry instead of total prior command count.
                let relevant_ranges: Vec<(usize, usize)> = if in_isolated_scope {
                    let scope = self.isolated_scope_stack.last_mut().expect("in scope");
                    let scope_cmds = &all_commands[backdrop_start_idx..scope_end_idx];
                    let index = scope.backdrop_index.get_or_insert_with(|| {
                        BackdropIndex::build(
                            scope_cmds,
                            self.page_rect,
                            self.font_cache,
                            &mut self.soft_mask_cache,
                            &mut self.stats,
                        )
                    });
                    let rel_limit = cmd_idx.saturating_sub(backdrop_start_idx);
                    index
                        .relevant_items_for_rect(target_rect)
                        .into_iter()
                        .filter_map(|id| {
                            let item = &index.items[id];
                            (item.start < rel_limit
                                && item
                                    .bounds
                                    .is_none_or(|b| intersect_rect(b, target_rect).is_some()))
                            .then_some((
                                backdrop_start_idx + item.start,
                                (backdrop_start_idx + item.end).min(cmd_idx - 1),
                            ))
                        })
                        .collect()
                } else {
                    let index = self.page_backdrop_index.get_or_insert_with(|| {
                        BackdropIndex::build(
                            all_commands,
                            self.page_rect,
                            self.font_cache,
                            &mut self.soft_mask_cache,
                            &mut self.stats,
                        )
                    });
                    index
                        .relevant_items_for_rect(target_rect)
                        .into_iter()
                        .filter_map(|id| {
                            let item = &index.items[id];
                            (item.start < cmd_idx
                                && item
                                    .bounds
                                    .is_none_or(|b| intersect_rect(b, target_rect).is_some()))
                            .then_some((item.start, item.end.min(cmd_idx - 1)))
                        })
                        .collect()
                };

                if relevant_ranges.is_empty() {
                    let (rgb, alpha) =
                        resolve_overprint_on_premul_rgba(empty_bg_rgba, &op, effective_alpha)?;
                    ResolvedOverprintSample {
                        rgb,
                        alpha,
                        uniform_bg_px: Some(empty_bg_rgba),
                    }
                } else {
                    let max_dim = target_rect.width().max(target_rect.height());
                    let Some(scale) = fit_scale_to_max_dim(max_dim) else {
                        return resolve_overprint_on_premul_rgba(
                            empty_bg_rgba,
                            &op,
                            effective_alpha,
                        );
                    };
                    let bg_color = if in_isolated_scope {
                        zpdf_display_list::Color::rgba(0.0, 0.0, 0.0, 0.0)
                    } else {
                        zpdf_display_list::Color::white()
                    };
                    let mut renderer = zpdf_render_cpu::CpuRenderer::new()
                        .with_fonts(self.font_cache)
                        .with_images(self.image_cache)
                        .with_render_budget(self.render_budget);
                    renderer
                        .begin_page(&PageRenderInfo {
                            page_rect: target_rect,
                            scale,
                            background: bg_color,
                        })
                        .map_err(|e| {
                            zpdf_core::Error::StreamDecode(format!(
                                "PPTX overprint backdrop resolve: {e}"
                            ))
                        })?;
                    let actual_scale = renderer.current_scale();
                    let mut rebaser = SoftMaskRebaser::new(
                        target_rect.x0,
                        target_rect.y1,
                        1.0,
                        1.0,
                        actual_scale,
                    )
                    .with_target_rect(target_rect);

                    let clip_prefix_len = initial_clip_len.min(self.clip_stack.len());
                    for &clip in &self.clip_stack[..clip_prefix_len] {
                        let prepared = rebaser.prepare_command(clip);
                        let exec_res = renderer.execute(prepared.as_ref()).map_err(|e| {
                            zpdf_core::Error::StreamDecode(format!(
                                "PPTX overprint backdrop resolve: {e}"
                            ))
                        });
                        if rebaser.take_needs_cpu_mask_cache_invalidation() {
                            renderer.clear_soft_mask_cache();
                        }
                        drop(prepared);
                        exec_res?;
                    }

                    let mut non_passthrough_stack: Vec<bool> = Vec::new();
                    for (r_start, r_end) in relevant_ranges {
                        for prev_cmd in &all_commands[r_start..=r_end] {
                            track_blend_non_passthrough(prev_cmd, &mut non_passthrough_stack);
                            if let RenderCommand::DrawGlyphRun(r) = prev_cmd {
                                if !self
                                    .font_cache
                                    .get(r.font_id)
                                    .is_some_and(|f| f.has_font_data())
                                {
                                    continue;
                                }
                            }
                            let prepared = rebaser.prepare_command(prev_cmd);
                            let exec_res = renderer.execute(prepared.as_ref()).map_err(|e| {
                                zpdf_core::Error::StreamDecode(format!(
                                    "PPTX overprint backdrop resolve: {e}"
                                ))
                            });
                            if rebaser.take_needs_cpu_mask_cache_invalidation() {
                                renderer.clear_soft_mask_cache();
                            }
                            drop(prepared);
                            exec_res?;
                            self.stats.backdrop_command_visits += 1;
                            if renderer.is_over_budget() {
                                return Err(zpdf_core::Error::StreamDecode(
                                    "PPTX overprint backdrop resolve exceeded time budget".into(),
                                ));
                            }
                        }
                    }
                    if non_passthrough_stack.iter().any(|&np| np) {
                        return Err(zpdf_core::Error::StreamDecode(
                            "PPTX overprinting native-only text sits inside an unclosed non-passthrough group".into(),
                        ));
                    }
                    let surf = renderer.current_surface().ok_or_else(|| {
                        zpdf_core::Error::StreamDecode(
                            "PPTX overprint backdrop surface unavailable".into(),
                        )
                    })?;
                    resolve_uniform_overprint_in_surface(
                        surf,
                        target_rect,
                        actual_scale,
                        target_rect,
                        &op,
                        effective_alpha,
                        empty_bg_rgba,
                    )?
                }
            }
        };

        let ResolvedOverprintSample {
            rgb: uniform_rgb,
            alpha: uniform_alpha,
            uniform_bg_px,
        } = sample;

        // `CpuRenderer` skips earlier native-only `TextBox` elements in the same
        // isolation scope; verify via spatial index that compositing any earlier
        // native-only text onto the sampled backdrop yields the exact same
        // resolved overprint color and opacity inside `target_rect`.
        struct OverlappingEarlierText {
            overlap_rect: Option<Rect>,
            translucent_changes_bg: bool,
        }
        let mut overlap_err: Option<zpdf_core::Error> = None;
        let mut overlapping_records: Vec<OverlappingEarlierText> = Vec::new();
        let uniform_alpha_u8 = (uniform_alpha * 255.0).round().clamp(0.0, 255.0) as u8;
        self.native_only_texts
            .visit_overlapping(Some(target_rect), &mut self.stats, |_, rec| {
                if overlap_err.is_none() && rec.cmd_idx >= backdrop_start_idx {
                    let rec_a = (rec.resolved_alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
                    if rec_a == 0 {
                        return true;
                    }
                    if rec.occluded_regions.overlaps_rect(target_rect) {
                        overlap_err = Some(zpdf_core::Error::StreamDecode(
                            "PPTX overprinting native-only text overlaps earlier native-only text occluded by intervening paint".into(),
                        ));
                        return true;
                    }
                    let mut translucent_changes_bg = false;
                    let comp_px = if rec_a == 255 {
                        [
                            rec.resolved_rgb.0,
                            rec.resolved_rgb.1,
                            rec.resolved_rgb.2,
                            255,
                        ]
                    } else if let Some(bg_px) = uniform_bg_px {
                        let c = source_over_premul(rec.resolved_rgb, rec.resolved_alpha, bg_px);
                        if c != bg_px {
                            translucent_changes_bg = true;
                        }
                        c
                    } else {
                        overlap_err = Some(zpdf_core::Error::StreamDecode(
                            "PPTX overprinting native-only text overlaps earlier translucent native-only text on a non-uniform backdrop".into(),
                        ));
                        return true;
                    };
                    match resolve_overprint_on_premul_rgba(comp_px, &op, effective_alpha) {
                        Ok((over_rgb, over_alpha)) => {
                            let over_a_u8 = (over_alpha * 255.0).round().clamp(0.0, 255.0) as u8;
                            if over_rgb != uniform_rgb || over_a_u8 != uniform_alpha_u8 {
                                overlap_err = Some(zpdf_core::Error::StreamDecode(
                                    "PPTX overprinting native-only text overlaps earlier native-only text with a different resolved color".into(),
                                ));
                            } else {
                                let overlap_rect =
                                    rec.bounds.and_then(|b| intersect_rect(b, target_rect));
                                overlapping_records.push(OverlappingEarlierText {
                                    overlap_rect,
                                    translucent_changes_bg,
                                });
                            }
                        }
                        Err(err) => {
                            overlap_err = Some(err);
                        }
                    }
                }
                true
            });
        if let Some(err) = overlap_err {
            return Err(err);
        }
        // Only reject multiple earlier native-only records when two records can
        // cover the SAME point inside `target_rect` and at least one is translucent
        // and changes the backdrop (so their stacked composite is unresolved).
        if overlapping_records.len() > 1
            && overlapping_records.iter().any(|r| r.translucent_changes_bg)
        {
            let mut unbounded: Vec<bool> = Vec::new();
            let mut bounded: Vec<(Rect, bool)> = Vec::new();
            for r in overlapping_records {
                if let Some(rect) = r.overlap_rect {
                    bounded.push((rect.normalize(), r.translucent_changes_bg));
                } else {
                    unbounded.push(r.translucent_changes_bg);
                }
            }
            let unbounded_conflicts = !unbounded.is_empty()
                && (unbounded.len() > 1 || !bounded.is_empty())
                && (unbounded.iter().any(|&t| t) || bounded.iter().any(|&(_, t)| t));
            let mut stacked_conflict = unbounded_conflicts;
            if !stacked_conflict && bounded.len() > 1 {
                stacked_conflict = has_stacked_translucent_rect_overlap(
                    &bounded,
                    &mut self.stats.native_text_record_visits,
                );
            }
            if stacked_conflict {
                return Err(zpdf_core::Error::StreamDecode(
                    "PPTX overprinting native-only text overlaps stacked earlier native-only texts with unresolved composited backdrop".into(),
                ));
            }
        }

        Ok((uniform_rgb, uniform_alpha))
    }

    fn emit_native_only_glyph_run(
        &mut self,
        all_commands: &[RenderCommand],
        cmd_idx: usize,
        glyph_run: &zpdf_display_list::GlyphRun,
    ) -> Result<()> {
        if glyph_run.glyphs.is_empty() {
            return Ok(());
        }
        let bounds = native_text_ink_bounds(glyph_run, self.font_cache);
        if !is_rect_visible_under_clips(bounds, &self.clip_stack, self.page_rect) {
            return Ok(());
        }
        if self.clip_stack_constrains_rect(bounds) {
            return Err(zpdf_core::Error::StreamDecode(
                "PPTX clipped native-only text requires font outlines and cannot be rendered faithfully".into(),
            ));
        }
        let (paint_override, resolved_rgb, resolved_alpha, is_opaque_black, is_opaque_white) =
            if let Some(op) = glyph_run.overprint.filter(|op| op.active != 0) {
                let (rgb, alpha) =
                    self.resolve_native_only_overprint_paint(all_commands, cmd_idx, glyph_run, op)?;
                let is_black = alpha >= 1.0 - 1e-3
                    && rgb == (0, 0, 0)
                    && op.paints(3)
                    && op.cmyk[3] >= 1.0 - 1e-3;
                let is_white = alpha >= 1.0 - 1e-3 && rgb == (255, 255, 255);
                (Some((rgb, alpha)), rgb, alpha, is_black, is_white)
            } else {
                let rgb = match &glyph_run.paint {
                    zpdf_display_list::Paint::Solid(c) => color_to_rgb_u8(c),
                    _ => (0, 0, 0),
                };
                let alpha = effective_paint_alpha(&glyph_run.paint, glyph_run.alpha);
                (
                    None,
                    rgb,
                    alpha,
                    glyph_run_is_opaque_black(glyph_run),
                    glyph_run_is_opaque_white(glyph_run),
                )
            };
        self.note_paint_over_native_texts(bounds, None, is_opaque_black, is_opaque_white);
        if self.convert_glyph_run_with_paint(glyph_run, false, paint_override)? {
            let element_idx = self.elements.len() - 1;
            self.native_only_texts.insert(NativeOnlyTextRecord {
                element_idx,
                cmd_idx,
                bounds,
                resolved_rgb,
                resolved_alpha,
                is_opaque_black,
                is_opaque_white,
                occluded_regions: MissingTextCoverage::None,
                alive: true,
                last_stamp: 0,
            });
        }
        Ok(())
    }

    fn note_paint_over_native_texts(
        &mut self,
        bounds: Option<Rect>,
        opaque_cover_rect: Option<Rect>,
        paint_is_opaque_black: bool,
        paint_is_opaque_white: bool,
    ) {
        if !self.native_only_texts.is_active() {
            return;
        }
        self.native_only_texts
            .visit_overlapping(bounds, &mut self.stats, |_, rec| {
                if let (Some(cover), Some(tb)) = (opaque_cover_rect, rec.bounds) {
                    if rect_contains(cover, tb) {
                        // Completely buried beneath an opaque normal rectangle on both
                        // the slide and `CpuRenderer`'s backdrop surface.
                        return false;
                    }
                }
                if !((rec.is_opaque_black && paint_is_opaque_black)
                    || (rec.is_opaque_white && paint_is_opaque_white))
                {
                    match (bounds, rec.bounds) {
                        (Some(pb), Some(tb)) => {
                            if let Some(inter) = intersect_rect(pb, tb) {
                                rec.occluded_regions.union_rect(inter);
                            }
                        }
                        _ => {
                            rec.occluded_regions = MissingTextCoverage::Unbounded;
                        }
                    }
                }
                true
            });
    }

    fn promote_native_only_texts(&mut self, record_indices: &mut [usize]) {
        if record_indices.is_empty() {
            return;
        }
        record_indices.sort_unstable_by_key(|&i| self.native_only_texts.records[i].element_idx);
        for &idx in record_indices.iter() {
            let old_slot = self.native_only_texts.records[idx].element_idx;
            let elem = std::mem::replace(&mut self.elements[old_slot], dummy_slide_element());
            self.removed_element_slots.push(old_slot);
            let new_slot = self.elements.len();
            self.elements.push(elem);
            self.native_only_texts.records[idx].element_idx = new_slot;
        }
    }

    fn compact_removed_elements(&mut self) {
        if self.removed_element_slots.is_empty() {
            return;
        }
        self.removed_element_slots.sort_unstable();
        self.removed_element_slots.dedup();
        let mut drop_pos = 0usize;
        let mut out_idx = 0usize;
        self.elements.retain(|_| {
            let keep = if drop_pos < self.removed_element_slots.len()
                && self.removed_element_slots[drop_pos] == out_idx
            {
                drop_pos += 1;
                false
            } else {
                true
            };
            out_idx += 1;
            keep
        });
        self.removed_element_slots.clear();
    }

    fn check_flattened_slice_missing_outlines(
        &mut self,
        group_cmds: &[RenderCommand],
        initial_clips: &[&RenderCommand],
        visible_region: Rect,
    ) -> Result<()> {
        let mut clips: Vec<&RenderCommand> = initial_clips.to_vec();
        let mut idx = 0usize;
        while idx < group_cmds.len() {
            let cmd = &group_cmds[idx];
            match cmd {
                RenderCommand::PushClip { .. } | RenderCommand::PushClipStroke { .. } => {
                    clips.push(cmd);
                    idx += 1;
                }
                RenderCommand::PopClip => {
                    clips.pop();
                    idx += 1;
                }
                RenderCommand::PushBlendGroup { alpha, mask, .. } => {
                    if !alpha.is_finite() || *alpha <= 0.0 {
                        if let Some(sub_end) = Self::find_matching_pop_blend(group_cmds, idx) {
                            idx = sub_end + 1;
                            continue;
                        }
                        break;
                    }
                    let sub_analysis = self.soft_mask_cache.analyze(
                        mask.as_ref(),
                        self.font_cache,
                        &mut self.stats,
                    );
                    if sub_analysis.is_proven_empty() {
                        if let Some(sub_end) = Self::find_matching_pop_blend(group_cmds, idx) {
                            idx = sub_end + 1;
                            continue;
                        }
                        break;
                    }
                    let sub_end = Self::find_matching_pop_blend(group_cmds, idx)
                        .unwrap_or(group_cmds.len() - 1);
                    if group_overlaps_missing_mask_text(
                        &sub_analysis.missing_text,
                        &group_cmds[idx..=sub_end],
                        &clips,
                        visible_region,
                        self.font_cache,
                        &mut self.soft_mask_cache,
                        &mut self.stats,
                    ) {
                        return Err(zpdf_core::Error::StreamDecode(
                            "PPTX soft mask contains text without font outlines and cannot be rendered faithfully".into(),
                        ));
                    }
                    match sub_analysis.renderable {
                        RenderableMaskBounds::Empty => {
                            idx = sub_end + 1;
                        }
                        RenderableMaskBounds::Bounded(mr) => {
                            if let Some(sub_vis) = intersect_rect(mr, visible_region) {
                                if sub_end > idx + 1 {
                                    self.check_flattened_slice_missing_outlines(
                                        &group_cmds[idx + 1..sub_end],
                                        &clips,
                                        sub_vis,
                                    )?;
                                }
                            }
                            idx = sub_end + 1;
                        }
                        RenderableMaskBounds::Unbounded => {
                            idx += 1;
                        }
                    }
                }
                RenderCommand::DrawGlyphRun(run)
                    if is_visible_native_only_glyph_run(cmd, self.font_cache)
                        && is_rect_visible_under_clips(
                            native_text_ink_bounds(run, self.font_cache),
                            &clips,
                            visible_region,
                        ) =>
                {
                    return Err(zpdf_core::Error::StreamDecode(
                        "PPTX flattened group contains native-only text without font outlines and cannot be rendered faithfully".into(),
                    ));
                }
                _ => {
                    idx += 1;
                }
            }
        }
        Ok(())
    }

    /// Inspect a group that is about to be rasterized via `CpuRenderer` for any
    /// visible metrics-only (`!has_font_data()`) `DrawGlyphRun` commands (or
    /// nested soft masks lacking font outlines) that `CpuRenderer` would
    /// silently skip.
    ///
    /// For an opaque isolated Normal group flattened solely because of inner
    /// overprint/blend operations, returns the surviving ordinary metrics-only
    /// glyph runs together with their enclosing isolated Normal scope chain
    /// so they can be emitted as native `TextBox` elements immediately after the
    /// flattened image. Nested Type 3 / pattern artwork runs and unfaithful
    /// overlaps return `Err`.
    fn plan_flattened_group_native_texts(
        &mut self,
        display_list: &DisplayList,
        group_start_idx: usize,
        group_cmds: &[RenderCommand],
        outer_bounds: Rect,
        is_opaque_isolated_normal: bool,
        outer_mask_renderable: RenderableMaskBounds,
    ) -> Result<Vec<PlannedNativeText>> {
        if matches!(outer_mask_renderable, RenderableMaskBounds::Empty) {
            return Ok(Vec::new());
        }
        let mut has_native_or_missing_mask = false;
        let mut scan_idx = 0usize;
        while scan_idx < group_cmds.len() {
            let c = &group_cmds[scan_idx];
            if is_visible_native_only_glyph_run(c, self.font_cache) {
                has_native_or_missing_mask = true;
                break;
            }
            if let RenderCommand::PushBlendGroup { alpha, mask, .. } = c {
                if !alpha.is_finite() || *alpha <= 0.0 {
                    if let Some(sub_end) = Self::find_matching_pop_blend(group_cmds, scan_idx) {
                        scan_idx = sub_end + 1;
                        continue;
                    }
                    break;
                }
                if let Some(m) = mask {
                    let sub_a =
                        self.soft_mask_cache
                            .analyze(Some(m), self.font_cache, &mut self.stats);
                    if !matches!(sub_a.missing_text, MissingTextCoverage::None) {
                        has_native_or_missing_mask = true;
                        break;
                    }
                }
            }
            scan_idx += 1;
        }
        if !has_native_or_missing_mask {
            return Ok(Vec::new());
        }

        let visible_region = match outer_mask_renderable {
            RenderableMaskBounds::Empty => return Ok(Vec::new()),
            RenderableMaskBounds::Bounded(mr) => match intersect_rect(mr, self.page_rect) {
                Some(r) => r,
                None => return Ok(Vec::new()),
            },
            RenderableMaskBounds::Unbounded => self.page_rect,
        };

        if !is_opaque_isolated_normal {
            let initial_clips = self.clip_stack.clone();
            self.check_flattened_slice_missing_outlines(
                group_cmds,
                &initial_clips,
                visible_region,
            )?;
            return Ok(Vec::new());
        }

        // Opaque isolated Normal group: walk its body (skipping outer Push/Pop).
        struct InnerNativeText {
            planned: PlannedNativeText,
            bounds: Option<Rect>,
            is_opaque_black: bool,
            is_opaque_white: bool,
        }

        // Track active clips pushed inside `group_cmds` (by absolute command index)
        // alongside the full clip stack so child isolated scopes carry their exact
        // outer-group clip prefix when emitted.
        let mut clips: Vec<&RenderCommand> = self.clip_stack.clone();
        let mut extra_clip_indices: Vec<usize> = Vec::new();
        let outer_pop_abs = group_start_idx + group_cmds.len().saturating_sub(1);
        let mut scope_stack: Vec<PlannedScope> = vec![PlannedScope {
            body_start: group_start_idx + 1,
            pop_idx: outer_pop_abs,
            bounds: outer_bounds,
            extra_clips: Vec::new(),
        }];
        let mut inner_texts: Vec<InnerNativeText> = Vec::new();
        let end_body = group_cmds.len().saturating_sub(1);
        let mut rel = 1usize;
        while rel < end_body {
            let cmd = &group_cmds[rel];
            match cmd {
                RenderCommand::PushClip { .. } | RenderCommand::PushClipStroke { .. } => {
                    clips.push(cmd);
                    extra_clip_indices.push(group_start_idx + rel);
                    rel += 1;
                }
                RenderCommand::PopClip => {
                    clips.pop();
                    extra_clip_indices.pop();
                    rel += 1;
                }
                RenderCommand::PushBlendGroup {
                    blend_mode,
                    isolated,
                    knockout,
                    alpha,
                    mask,
                    bounds,
                } => {
                    let group_alpha = if alpha.is_finite() {
                        alpha.clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    let passthrough = !*isolated
                        && !*knockout
                        && *blend_mode == zpdf_display_list::BlendMode::Normal
                        && group_alpha >= 1.0
                        && mask.is_none();
                    if passthrough {
                        rel += 1;
                        continue;
                    }
                    let Some(sub_end) = Self::find_matching_pop_blend(group_cmds, rel) else {
                        return Err(zpdf_core::Error::StreamDecode(
                            "PPTX isolated group contains unclosed blend around native-only text"
                                .into(),
                        ));
                    };
                    let sub_slice = &group_cmds[rel..=sub_end];
                    if group_alpha <= 0.0 {
                        rel = sub_end + 1;
                        continue;
                    }
                    let sub_mask_analysis = self.soft_mask_cache.analyze(
                        mask.as_ref(),
                        self.font_cache,
                        &mut self.stats,
                    );
                    if sub_mask_analysis.is_proven_empty() {
                        rel = sub_end + 1;
                        continue;
                    }
                    if group_overlaps_missing_mask_text(
                        &sub_mask_analysis.missing_text,
                        sub_slice,
                        &clips,
                        visible_region,
                        self.font_cache,
                        &mut self.soft_mask_cache,
                        &mut self.stats,
                    ) {
                        return Err(zpdf_core::Error::StreamDecode(
                            "PPTX soft mask contains text without font outlines and cannot be rendered faithfully".into(),
                        ));
                    }
                    if matches!(sub_mask_analysis.renderable, RenderableMaskBounds::Empty) {
                        rel = sub_end + 1;
                        continue;
                    }
                    let sub_is_transparent_normal = *blend_mode
                        == zpdf_display_list::BlendMode::Normal
                        && !*knockout
                        && group_alpha >= 1.0
                        && mask.is_none()
                        && !sub_slice[1..sub_slice.len().saturating_sub(1)]
                            .iter()
                            .any(|c| {
                                matches!(
                                    c,
                                    RenderCommand::PushBlendGroup { blend_mode, alpha, .. }
                                        if *blend_mode != zpdf_display_list::BlendMode::Normal
                                            && alpha.is_finite()
                                            && *alpha > 0.0
                                ) || command_has_renderable_overprint(c, self.font_cache)
                            });
                    if sub_is_transparent_normal {
                        if *isolated {
                            scope_stack.push(PlannedScope {
                                body_start: group_start_idx + rel + 1,
                                pop_idx: group_start_idx + sub_end,
                                bounds: *bounds,
                                extra_clips: extra_clip_indices.clone(),
                            });
                        }
                        rel += 1;
                        continue;
                    }
                    let sub_visible_region = match sub_mask_analysis.renderable {
                        RenderableMaskBounds::Bounded(mr) => intersect_rect(mr, visible_region),
                        RenderableMaskBounds::Unbounded => Some(visible_region),
                        RenderableMaskBounds::Empty => None,
                    };
                    if let Some(sub_vis_rect) = sub_visible_region {
                        self.check_flattened_slice_missing_outlines(
                            sub_slice,
                            &clips,
                            sub_vis_rect,
                        )?;
                    }
                    if !inner_texts.is_empty() {
                        let sub_bounds =
                            match compute_commands_bounds(sub_slice, (0.0, 0.0), self.font_cache) {
                                CommandBounds::Empty => None,
                                CommandBounds::Known(r) => tighten_rect_to_clips(r, &clips),
                                CommandBounds::Unknown => {
                                    tighten_rect_to_clips(*bounds, &clips).or(Some(self.page_rect))
                                }
                            };
                        if let Some(sb) = sub_bounds {
                            let preserves_black =
                                group_preserves_opaque_black(sub_slice, self.font_cache);
                            let preserves_white =
                                group_preserves_opaque_white(sub_slice, self.font_cache);
                            for rec in &inner_texts {
                                let overlaps =
                                    rec.bounds.is_none_or(|tb| intersect_rect(tb, sb).is_some());
                                if overlaps
                                    && !((rec.is_opaque_black && preserves_black)
                                        || (rec.is_opaque_white && preserves_white))
                                {
                                    return Err(zpdf_core::Error::StreamDecode(
                                        "PPTX isolated group overlaps native-only text without font outlines and cannot be rendered faithfully".into(),
                                    ));
                                }
                            }
                        }
                    }
                    rel = sub_end + 1;
                }
                RenderCommand::PopBlendGroup => {
                    let abs_idx = group_start_idx + rel;
                    if scope_stack.len() > 1
                        && scope_stack
                            .last()
                            .is_some_and(|scope| scope.pop_idx == abs_idx)
                    {
                        scope_stack.pop();
                    }
                    rel += 1;
                }
                RenderCommand::DrawGlyphRun(run)
                    if is_visible_native_only_glyph_run(cmd, self.font_cache) =>
                {
                    let bounds = native_text_ink_bounds(run, self.font_cache);
                    if !is_rect_visible_under_clips(bounds, &clips, visible_region) {
                        rel += 1;
                        continue;
                    }
                    let abs_idx = group_start_idx + rel;
                    if display_list.is_in_type3_glyph_range(abs_idx)
                        || display_list.is_in_pattern_range(abs_idx)
                    {
                        return Err(zpdf_core::Error::StreamDecode(
                            "PPTX glyph artwork contains nested text without font outlines and cannot be rasterized".into(),
                        ));
                    }
                    if clip_stack_constrains_rect_for(&clips, bounds) {
                        return Err(zpdf_core::Error::StreamDecode(
                            "PPTX isolated group clips native-only text without font outlines and cannot be rendered faithfully".into(),
                        ));
                    }
                    // Surviving native-only runs are emitted after the Form PNG
                    // in their exact original source order, so overlapping runs
                    // with different colors preserve relative Z-order naturally
                    // whenever no raster paint crosses between them.
                    inner_texts.push(InnerNativeText {
                        planned: PlannedNativeText {
                            rel_idx: rel,
                            scope_chain: scope_stack.clone(),
                        },
                        bounds,
                        is_opaque_black: glyph_run_is_opaque_black(run),
                        is_opaque_white: glyph_run_is_opaque_white(run),
                    });
                    rel += 1;
                }
                other => {
                    if !inner_texts.is_empty() && is_visible_paint_command(other, self.font_cache) {
                        let cmd_bounds = match compute_commands_bounds(
                            std::slice::from_ref(other),
                            (0.0, 0.0),
                            self.font_cache,
                        ) {
                            CommandBounds::Empty => {
                                rel += 1;
                                continue;
                            }
                            CommandBounds::Known(r) => match tighten_rect_to_clips(r, &clips) {
                                Some(t) => Some(t),
                                None => {
                                    rel += 1;
                                    continue;
                                }
                            },
                            CommandBounds::Unknown => None,
                        };
                        let cover = opaque_normal_fill_cover_rect(other, &clips);
                        let slice = std::slice::from_ref(other);
                        let preserves_black = group_preserves_opaque_black(slice, self.font_cache);
                        let preserves_white = group_preserves_opaque_white(slice, self.font_cache);
                        let mut unfaithful = false;
                        inner_texts.retain(|rec| {
                            if let (Some(c), Some(tb)) = (cover, rec.bounds) {
                                if rect_contains(c, tb) {
                                    return false;
                                }
                            }
                            let overlaps = match (rec.bounds, cmd_bounds) {
                                (Some(tb), Some(pb)) => intersect_rect(tb, pb).is_some(),
                                _ => true,
                            };
                            if overlaps
                                && !((rec.is_opaque_black && preserves_black)
                                    || (rec.is_opaque_white && preserves_white))
                            {
                                unfaithful = true;
                            }
                            true
                        });
                        if unfaithful {
                            return Err(zpdf_core::Error::StreamDecode(
                                "PPTX isolated group overlaps native-only text without font outlines and cannot be rendered faithfully".into(),
                            ));
                        }
                    }
                    rel += 1;
                }
            }
        }

        Ok(inner_texts.into_iter().map(|t| t.planned).collect())
    }

    fn clip_stack_constrains_commands(&self, cmds: &[RenderCommand]) -> bool {
        if self.clip_stack.is_empty() {
            return false;
        }
        match compute_commands_bounds(cmds, (0.0, 0.0), self.font_cache) {
            CommandBounds::Empty => false,
            CommandBounds::Known(r) => self.clip_stack_constrains_rect(Some(r)),
            CommandBounds::Unknown => true,
        }
    }

    fn track_clip(&mut self, cmd: &'a RenderCommand) {
        match cmd {
            RenderCommand::PushClip { .. } | RenderCommand::PushClipStroke { .. } => {
                self.clip_stack.push(cmd);
                self.shared_clip_stack = None;
            }
            RenderCommand::PopClip => {
                self.clip_stack.pop();
                self.shared_clip_stack = None;
            }
            _ => {}
        }
    }

    fn snapshot_clip_stack(&mut self, count_allocation: bool) -> std::rc::Rc<[&'a RenderCommand]> {
        if let Some(shared) = &self.shared_clip_stack {
            return std::rc::Rc::clone(shared);
        }
        let shared: std::rc::Rc<[&'a RenderCommand]> =
            std::rc::Rc::from(self.clip_stack.as_slice());
        self.shared_clip_stack = Some(std::rc::Rc::clone(&shared));
        if count_allocation && !self.clip_stack.is_empty() {
            self.stats.queued_clip_stack_allocations += 1;
        }
        shared
    }

    fn find_matching_pop_blend(commands: &[RenderCommand], push_idx: usize) -> Option<usize> {
        let mut depth = 1usize;
        for (idx, cmd) in commands.iter().enumerate().skip(push_idx + 1) {
            match cmd {
                RenderCommand::PushBlendGroup { .. } => depth += 1,
                RenderCommand::PopBlendGroup => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(idx);
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn find_matching_pop_clip(commands: &[RenderCommand], push_idx: usize) -> Option<usize> {
        let mut clip_depth = 1usize;
        let mut blend_depth = 0usize;
        for (idx, cmd) in commands.iter().enumerate().skip(push_idx + 1) {
            match cmd {
                RenderCommand::PushClip { .. } | RenderCommand::PushClipStroke { .. } => {
                    clip_depth += 1;
                }
                RenderCommand::PopClip => {
                    clip_depth -= 1;
                    if clip_depth == 0 {
                        return (blend_depth == 0).then_some(idx);
                    }
                }
                RenderCommand::PushBlendGroup { .. } => blend_depth += 1,
                RenderCommand::PopBlendGroup => {
                    if blend_depth == 0 {
                        return None;
                    }
                    blend_depth -= 1;
                }
                _ => {}
            }
        }
        None
    }

    fn canonical_axis_window(
        a0: f64,
        a1: f64,
        origin: f64,
        page_max: f64,
    ) -> Option<(u8, i32, f64, f64)> {
        let len = a1 - a0;
        if !(a0.is_finite() && a1.is_finite() && len.is_finite() && len >= 0.0) {
            return None;
        }
        // Tier 0: fits completely inside a single aligned 512pt grid cell (zero overlap).
        if len <= BACKDROP_TILE_PT {
            let g0 = ((a0 - origin) / BACKDROP_TILE_PT).floor();
            let g1 = if a1 > a0 {
                ((a1 - origin) / BACKDROP_TILE_PT).ceil() - 1.0
            } else {
                g0
            };
            if g0 == g1 && g0.is_finite() && g0 >= i32::MIN as f64 && g0 <= i32::MAX as f64 {
                let w0 = (origin + g0 * BACKDROP_TILE_PT).max(origin);
                let w1 = (origin + (g0 + 1.0) * BACKDROP_TILE_PT).min(page_max);
                if w0 <= a0 + 1e-6 && a1 <= w1 + 1e-6 {
                    return Some((0, g0 as i32, w0, w1));
                }
            }
            // Tier 1: <= 512pt crop straddling a 512pt grid line -> half-stepped 1024pt
            // window (step 512pt, win 1024pt; at most 2 windows overlap any point).
            let step = BACKDROP_TILE_PT;
            let win = BACKDROP_TILE_PT * 2.0;
            let g = ((a0 - origin) / step).floor();
            if g.is_finite() && g >= i32::MIN as f64 && g <= i32::MAX as f64 {
                let w0 = (origin + g * step).max(origin);
                let w1 = (origin + g * step + win).min(page_max);
                if w0 <= a0 + 1e-6 && a1 <= w1 + 1e-6 {
                    return Some((1, g as i32, w0, w1));
                }
            }
        }
        // Tier 2+: crops wider than 512pt (and boundary-precision fallbacks).
        // Window sizes double from 1024pt (1024, 2048, 4096, ...) with `step = win / 4`
        // so every interval of length <= `0.75 * win` fits inside one canonical window,
        // bounding overlapping windows per coordinate to <= 4 per tier.
        let mut win = BACKDROP_TILE_PT * 2.0;
        for tier in 2u8..=24u8 {
            let step = win * 0.25;
            if len <= win - step {
                let g = ((a0 - origin) / step).floor();
                if g.is_finite() && g >= i32::MIN as f64 && g <= i32::MAX as f64 {
                    let w0 = (origin + g * step).max(origin);
                    let w1 = (origin + g * step + win).min(page_max);
                    if w0 <= a0 + 1e-6 && a1 <= w1 + 1e-6 {
                        return Some((tier, g as i32, w0, w1));
                    }
                }
            }
            win *= 2.0;
        }
        None
    }

    fn choose_tile_window(&self, crop_rect: Rect) -> Option<(TileGroupKey, Rect, f32)> {
        let (tx_tier, gx, x0, x1) = Self::canonical_axis_window(
            crop_rect.x0,
            crop_rect.x1,
            self.page_rect.x0,
            self.page_rect.x1,
        )?;
        let (ty_tier, gy, y0, y1) = Self::canonical_axis_window(
            crop_rect.y0,
            crop_rect.y1,
            self.page_rect.y0,
            self.page_rect.y1,
        )?;
        let tile_rect = Rect::new(x0, y0, x1, y1);
        let tile_max_dim = tile_rect.width().max(tile_rect.height());
        let tile_scale = fit_scale_to_max_dim(tile_max_dim)?;
        Some((
            TileGroupKey {
                tx_tier,
                gx,
                ty_tier,
                gy,
            },
            tile_rect,
            tile_scale,
        ))
    }

    fn ensure_full_page_backdrop(
        &mut self,
        page_scale: f32,
    ) -> Result<Option<(Rect, f32, u32, u32)>> {
        if self.backdrop_mgr.full_page.is_none() {
            let Some(surface) = BackdropSurface::open(
                self.page_rect,
                page_scale,
                self.font_cache,
                self.image_cache,
                self.backdrop_mgr.render_budget,
            )?
            else {
                return Ok(None);
            };
            self.backdrop_mgr.full_page = Some(FullPageBackdrop {
                surface,
                cursor: 0,
                non_passthrough_stack: Vec::new(),
            });
            let live = self.live_backdrop_pixels();
            self.stats.peak_backdrop_pixels = self.stats.peak_backdrop_pixels.max(live);
        }
        let full = self.backdrop_mgr.full_page.as_ref().expect("initialized");
        Ok(Some((
            full.surface.rect,
            full.surface.scale,
            full.surface.width,
            full.surface.height,
        )))
    }

    #[allow(clippy::too_many_arguments)]
    fn enqueue_tiled_backdrop_snapshot(
        &mut self,
        start_idx: usize,
        end_idx: usize,
        crop_rect: Rect,
        key: TileGroupKey,
        tile_rect: Rect,
        tile_scale: f32,
        unfaithful_native_text_overlaps: Vec<Option<Rect>>,
    ) {
        let element_idx = self.elements.len();
        // Reserve exact paint-order Z-index in `self.elements`; filled or removed
        // when `flush_pending_tiled_snapshots` resolves each tile in a single pass.
        self.elements.push(dummy_slide_element());
        let snap = PendingTiledSnapshot {
            element_idx,
            start_idx,
            end_idx,
            clip_stack: self.snapshot_clip_stack(true),
            crop_rect,
            unfaithful_native_text_overlaps,
        };
        if let Some(&group_idx) = self.backdrop_mgr.grid_groups.get(&key) {
            self.backdrop_mgr.pending_groups[group_idx]
                .snapshots
                .push(snap);
            return;
        }
        let group_idx = self.backdrop_mgr.pending_groups.len();
        self.backdrop_mgr.grid_groups.insert(key, group_idx);
        self.backdrop_mgr.pending_groups.push(PendingTileGroup {
            tile_rect,
            tile_scale,
            snapshots: vec![snap],
        });
    }

    fn render_pass1_coverage(
        &mut self,
        group_cmds: &[RenderCommand],
        clip_stack: &[&RenderCommand],
        sub_render_rect: Rect,
        scale: f32,
    ) -> Result<zpdf_render_cpu::RenderedPage> {
        use zpdf_render::{PageRenderInfo, RenderBackend};

        let mut pass1_rebaser =
            SoftMaskRebaser::new(sub_render_rect.x0, sub_render_rect.y1, 1.0, 1.0, scale)
                .with_target_rect(sub_render_rect);
        let mut renderer = zpdf_render_cpu::CpuRenderer::new()
            .with_fonts(self.font_cache)
            .with_images(self.image_cache)
            .with_render_budget(self.render_budget);
        renderer
            .begin_page(&PageRenderInfo {
                page_rect: sub_render_rect,
                scale,
                background: zpdf_display_list::Color::rgba(0.0, 0.0, 0.0, 0.0),
            })
            .map_err(|e| zpdf_core::Error::StreamDecode(format!("PPTX mask render: {e}")))?;
        for &clip in clip_stack {
            renderer
                .execute(clip)
                .map_err(|e| zpdf_core::Error::StreamDecode(format!("PPTX mask render: {e}")))?;
            self.stats.pass1_command_visits += 1;
        }
        for cmd in group_cmds {
            let prepared = pass1_rebaser.prepare_command(cmd);
            let exec_res = renderer
                .execute(prepared.as_ref())
                .map_err(|e| zpdf_core::Error::StreamDecode(format!("PPTX mask render: {e}")));
            if pass1_rebaser.take_needs_cpu_mask_cache_invalidation() {
                renderer.clear_soft_mask_cache();
            }
            drop(prepared);
            exec_res?;
            self.stats.pass1_command_visits += 1;
        }
        for _ in 0..clip_stack.len() {
            renderer
                .execute(&RenderCommand::PopClip)
                .map_err(|e| zpdf_core::Error::StreamDecode(format!("PPTX mask render: {e}")))?;
        }
        if renderer.is_over_budget() {
            return Err(zpdf_core::Error::StreamDecode(
                "PPTX blend coverage render exceeded time budget".into(),
            ));
        }
        renderer
            .end_page()
            .map_err(|e| zpdf_core::Error::StreamDecode(format!("PPTX mask render: {e}")))
    }

    fn flush_pending_tiled_snapshots(&mut self, all_commands: &[RenderCommand]) -> Result<()> {
        if self.backdrop_mgr.pending_groups.is_empty() {
            return Ok(());
        }
        // Release any full-page or scope backdrop surfaces before opening tiled
        // surfaces so at most ONE backdrop surface is live at a time (<= 16 MiB cap).
        self.isolated_scope_stack.clear();
        self.backdrop_mgr.full_page = None;
        let index = self.page_backdrop_index.take().unwrap_or_else(|| {
            BackdropIndex::build(
                all_commands,
                self.page_rect,
                self.font_cache,
                &mut self.soft_mask_cache,
                &mut self.stats,
            )
        });
        let groups = std::mem::take(&mut self.backdrop_mgr.pending_groups);
        self.backdrop_mgr.grid_groups.clear();
        let mut empty_slots: Vec<usize> = Vec::new();

        for group in groups {
            let Some(first_snap) = group.snapshots.first() else {
                continue;
            };
            let mut union = first_snap.crop_rect;
            for snap in &group.snapshots[1..] {
                union = Rect::new(
                    union.x0.min(snap.crop_rect.x0),
                    union.y0.min(snap.crop_rect.y0),
                    union.x1.max(snap.crop_rect.x1),
                    union.y1.max(snap.crop_rect.y1),
                );
            }
            // Choose the highest permitted raster scale for the collected crop
            // union (up to MAX_MASK_RASTER_SCALE / MAX_MASK_RASTER_DIM) while
            // keeping the shared grouping window for O(N) coalescing, so e.g.
            // a single 800pt strip in a 2048pt window still renders at 2.0x.
            let snap_tight_rect = |s: f32| -> (Rect, u32, u32) {
                let gs = s as f64;
                let gx0 = ((union.x0 - group.tile_rect.x0) * gs).floor().max(0.0);
                let gy0 = ((group.tile_rect.y1 - union.y1) * gs).floor().max(0.0);
                let gx1 = ((union.x1 - group.tile_rect.x0) * gs).ceil().max(gx0 + 1.0);
                let gy1 = ((group.tile_rect.y1 - union.y0) * gs).ceil().max(gy0 + 1.0);
                let rect = Rect::new(
                    (group.tile_rect.x0 + gx0 / gs).max(group.tile_rect.x0),
                    (group.tile_rect.y1 - gy1 / gs).max(group.tile_rect.y0),
                    (group.tile_rect.x0 + gx1 / gs).min(group.tile_rect.x1),
                    (group.tile_rect.y1 - gy0 / gs).min(group.tile_rect.y1),
                );
                let w = (rect.width() * gs).ceil().max(1.0) as u32;
                let h = (rect.height() * gs).ceil().max(1.0) as u32;
                (rect, w, h)
            };
            let union_max_dim = union.width().max(union.height());
            let mut surface_scale = fit_scale_to_max_dim(union_max_dim)
                .map_or(group.tile_scale, |s| s.max(group.tile_scale));
            let (mut tight_rect, w, h) = snap_tight_rect(surface_scale);
            if w > MAX_MASK_RASTER_DIM as u32 || h > MAX_MASK_RASTER_DIM as u32 {
                let refit_dim = union_max_dim + 2.0 / surface_scale as f64;
                surface_scale = fit_scale_to_max_dim(refit_dim)
                    .map_or(group.tile_scale, |s| s.max(group.tile_scale));
                tight_rect = snap_tight_rect(surface_scale).0;
            }
            let Some(mut surface) = BackdropSurface::open(
                tight_rect,
                surface_scale,
                self.font_cache,
                self.image_cache,
                self.backdrop_mgr.render_budget,
            )?
            else {
                for snap in group.snapshots {
                    empty_slots.push(snap.element_idx);
                }
                continue;
            };
            self.stats.peak_backdrop_pixels = self.stats.peak_backdrop_pixels.max(surface.pixels());
            let relevant_items = index.relevant_items_for_rect(surface.rect);
            let mut item_pos = 0usize;

            for snap in group.snapshots {
                let s = surface.scale as f64;
                let px0 = ((snap.crop_rect.x0 - surface.rect.x0) * s).floor().max(0.0) as u32;
                let py0 = ((surface.rect.y1 - snap.crop_rect.y1) * s).floor().max(0.0) as u32;
                let px1 = (((snap.crop_rect.x1 - surface.rect.x0) * s).ceil().max(0.0) as u32)
                    .min(surface.width);
                let py1 = (((surface.rect.y1 - snap.crop_rect.y0) * s).ceil().max(0.0) as u32)
                    .min(surface.height);
                if px1 <= px0 || py1 <= py0 {
                    empty_slots.push(snap.element_idx);
                    continue;
                }
                let sub_w = px1 - px0;
                let sub_h = py1 - py0;
                let sub_x0 = surface.rect.x0 + px0 as f64 / s;
                let sub_y1 = surface.rect.y1 - py0 as f64 / s;
                let mut sub_x1 = surface.rect.x0 + px1 as f64 / s;
                while ((sub_x1 - sub_x0) * s).ceil() > sub_w as f64 && sub_x1 > sub_x0 {
                    sub_x1 = sub_x1.next_down();
                }
                let mut sub_y0 = surface.rect.y1 - py1 as f64 / s;
                while ((sub_y1 - sub_y0) * s).ceil() > sub_h as f64 && sub_y0 < sub_y1 {
                    sub_y0 = sub_y0.next_up();
                }
                let sub_render_rect = Rect::new(sub_x0, sub_y0, sub_x1, sub_y1);

                let mut rendered = self.render_pass1_coverage(
                    &all_commands[snap.start_idx..=snap.end_idx],
                    &snap.clip_stack,
                    sub_render_rect,
                    surface.scale,
                )?;
                if rendered.data.as_chunks::<4>().0.iter().all(|px| px[3] == 0) {
                    empty_slots.push(snap.element_idx);
                    continue;
                }
                if snap.unfaithful_native_text_overlaps.iter().any(|&target| {
                    pass1_touches_rect(&rendered, sub_render_rect, surface.scale, target)
                }) {
                    return Err(zpdf_core::Error::StreamDecode(
                        "PPTX backdrop export overlaps native-only text without font outlines and cannot render the overlap faithfully".into(),
                    ));
                }

                // Advance this tile's surface monotonically in a single forward
                // pass across all snapshots belonging to the tile — never
                // evicted or replayed from zero even when drawing cycles across
                // many tiles.
                let batch_start = surface.start_batch_deadline();
                let mut batch_err: Option<zpdf_core::Error> = None;
                'tile_items: while item_pos < relevant_items.len() {
                    let item = &index.items[relevant_items[item_pos]];
                    if item.start > snap.end_idx {
                        break;
                    }
                    if let Some(b) = item.bounds {
                        if intersect_rect(b, surface.rect).is_none() {
                            item_pos += 1;
                            continue;
                        }
                    }
                    for cmd in &all_commands[item.start..=item.end] {
                        if let Err(err) = surface.execute_command(cmd, &mut self.stats) {
                            batch_err = Some(err);
                            break 'tile_items;
                        }
                    }
                    item_pos += 1;
                }
                surface.finish_batch_deadline(batch_start);
                if let Some(err) = batch_err {
                    return Err(err);
                }

                composite_coverage_with_backdrop(
                    &mut rendered,
                    surface.renderer.current_surface(),
                    px0,
                    py0,
                    px1,
                    py1,
                    sub_w,
                    sub_h,
                );
                let png_data = rgba_to_png(rendered.width, rendered.height, &rendered.data)?;
                let img_x1 = sub_x1.min(self.page_rect.x1);
                let img_y0 = sub_y0.max(self.page_rect.y0);
                let width_ppt = (img_x1 - sub_x0).max(0.0);
                let height_ppt = (sub_y1 - img_y0).max(0.0);
                self.image_counter += 1;
                let image_id = format!("image{}", self.image_counter);
                let (x_ppt, y_ppt) = self.pdf_to_ppt_coords(sub_x0, sub_y1);
                self.elements[snap.element_idx] = SlideElement::Image {
                    x_emu: pt_to_emu(x_ppt),
                    y_emu: pt_to_emu(y_ppt),
                    width_emu: pt_to_emu(width_ppt),
                    height_emu: pt_to_emu(height_ppt),
                    image_data: png_data,
                    image_id,
                };
                self.stats.backdrop_groups_completed += 1;
                #[cfg(test)]
                if self
                    .expire_backdrop_after_groups
                    .is_some_and(|n| self.stats.backdrop_groups_completed >= n)
                {
                    surface.remaining_budget = Some(std::time::Duration::ZERO);
                    self.backdrop_mgr.expire_all_budgets();
                }
            }
        }

        self.removed_element_slots.extend(empty_slots);
        Ok(())
    }

    /// Rasterize a drawing group (soft-masked, clipped Type 3 artwork,
    /// knockout group, non-Normal blend group, or translucent Type 3 paint)
    /// onto a transparent bitmap so masks, clips, blend modes, knockout, and
    /// opacity survive PowerPoint export.
    ///
    /// When `backdrop_range` is `Some((all_commands, start_idx, end_idx))` (for
    /// non-Normal blend modes such as `Multiply`), resolves the blend against a
    /// persistent incremental backdrop (full-page on standard pages, or
    /// tile-batched single-pass crop-scale surfaces with spatial indexing on
    /// large sheets) and masks the output strictly to pixels touched by `group_cmds`.
    fn flatten_commands_group(
        &mut self,
        group_cmds: &[RenderCommand],
        mask: Option<&zpdf_display_list::SoftMask>,
        group_bounds: Rect,
        backdrop_range: Option<(&[RenderCommand], usize, usize)>,
    ) -> Result<()> {
        use zpdf_render::{PageRenderInfo, RenderBackend};

        let fallback_bounds =
            intersect_rect(group_bounds, self.page_rect).unwrap_or(self.page_rect);
        let mut raw_bounds = match self.compute_commands_bounds_cached(group_cmds)? {
            CommandBounds::Empty => return Ok(()),
            CommandBounds::Known(r) => Rect::new(r.x0 - 1.0, r.y0 - 1.0, r.x1 + 1.0, r.y1 + 1.0),
            CommandBounds::Unknown => fallback_bounds,
        };
        // Tighten to every active caller clip and any leading clip inside the
        // group first so disjoint clipped artwork exits immediately and missing
        // mask-font checks are restricted strictly to the clipped source area.
        for &clip in &self.clip_stack {
            let Some(tightened) = tighten_rect_to_clip(raw_bounds, clip) else {
                return Ok(());
            };
            raw_bounds = tightened;
        }
        if let Some(first) = group_cmds.first() {
            if matches!(
                first,
                RenderCommand::PushClip { .. } | RenderCommand::PushClipStroke { .. }
            ) {
                // Only tighten the crop to a leading local clip if its matching
                // PopClip occurs after all paint commands in `group_cmds`; if
                // paint follows the pop, keep the caller-tightened bounds and
                // let CpuRenderer replay the local clip and its pop.
                let covers_all_paint =
                    Self::find_matching_pop_clip(group_cmds, 0).is_some_and(|pop_idx| {
                        !group_cmds[pop_idx + 1..]
                            .iter()
                            .any(is_paint_render_command)
                    });
                if covers_all_paint {
                    let Some(tightened) = tighten_rect_to_clip(raw_bounds, first) else {
                        return Ok(());
                    };
                    raw_bounds = tightened;
                }
            }
        }
        let Some(clipped_source_rect) = intersect_rect(raw_bounds, self.page_rect) else {
            return Ok(());
        };
        let mask_analysis = self
            .soft_mask_cache
            .analyze(mask, self.font_cache, &mut self.stats);
        if mask_analysis.is_proven_empty() {
            return Ok(());
        }
        if group_overlaps_missing_mask_text(
            &mask_analysis.missing_text,
            group_cmds,
            &self.clip_stack,
            clipped_source_rect,
            self.font_cache,
            &mut self.soft_mask_cache,
            &mut self.stats,
        ) {
            return Err(zpdf_core::Error::StreamDecode(
                "PPTX soft mask contains text without font outlines and cannot be rendered faithfully".into(),
            ));
        }
        let crop_rect = match mask_analysis.renderable {
            RenderableMaskBounds::Empty => return Ok(()),
            RenderableMaskBounds::Bounded(padded) => {
                let Some(both) = intersect_rect(clipped_source_rect, padded) else {
                    return Ok(());
                };
                both
            }
            RenderableMaskBounds::Unbounded => clipped_source_rect,
        };
        let crop_w = crop_rect.width();
        let crop_h = crop_rect.height();
        let max_dim = crop_w.max(crop_h);
        let Some(crop_scale) = fit_scale_to_max_dim(max_dim) else {
            return Ok(());
        };

        if let Some((all_commands, start_idx, end_idx)) = backdrop_range {
            let page_max_dim = self.page_rect.width().max(self.page_rect.height());
            let Some(page_scale) = fit_scale_to_max_dim(page_max_dim) else {
                return Ok(());
            };
            if crop_scale > page_scale + 1e-6 {
                if let Some((key, tile_rect, tile_scale)) = self.choose_tile_window(crop_rect) {
                    // Small/medium/wide crop on a large sheet: batch snapshots
                    // on canonical hierarchical grid windows so every window
                    // renders in a single monotonic forward pass with bounded
                    // memory, linear command visits, and union-fitted scale.
                    let mut to_promote: Vec<usize> = Vec::new();
                    let mut unfaithful_overlaps: Vec<Option<Rect>> = Vec::new();
                    if self.native_only_texts.is_active() {
                        let preserves_black =
                            group_preserves_opaque_black(group_cmds, self.font_cache);
                        let preserves_white =
                            group_preserves_opaque_white(group_cmds, self.font_cache);
                        self.native_only_texts.visit_overlapping(
                            Some(crop_rect),
                            &mut self.stats,
                            |rec_idx, rec| {
                                let promotable = rec.occluded_regions.is_empty()
                                    && ((rec.is_opaque_black && preserves_black)
                                        || (rec.is_opaque_white && preserves_white));
                                if promotable {
                                    to_promote.push(rec_idx);
                                } else {
                                    unfaithful_overlaps.push(rec.bounds);
                                }
                                true
                            },
                        );
                    }
                    self.enqueue_tiled_backdrop_snapshot(
                        start_idx,
                        end_idx,
                        crop_rect,
                        key,
                        tile_rect,
                        tile_scale,
                        unfaithful_overlaps,
                    );
                    self.promote_native_only_texts(&mut to_promote);
                    return Ok(());
                }
            }

            let Some((bg_rect, bg_scale, bg_w, bg_h)) =
                self.ensure_full_page_backdrop(page_scale)?
            else {
                return Ok(());
            };
            let s = bg_scale as f64;
            let px0 = ((crop_rect.x0 - bg_rect.x0) * s).floor().max(0.0) as u32;
            let py0 = ((bg_rect.y1 - crop_rect.y1) * s).floor().max(0.0) as u32;
            let px1 = (((crop_rect.x1 - bg_rect.x0) * s).ceil().max(0.0) as u32).min(bg_w);
            let py1 = (((bg_rect.y1 - crop_rect.y0) * s).ceil().max(0.0) as u32).min(bg_h);
            if px1 <= px0 || py1 <= py0 {
                return Ok(());
            }
            let sub_w = px1 - px0;
            let sub_h = py1 - py0;
            let sub_x0 = bg_rect.x0 + px0 as f64 / s;
            let sub_y1 = bg_rect.y1 - py0 as f64 / s;
            let mut sub_x1 = bg_rect.x0 + px1 as f64 / s;
            while ((sub_x1 - sub_x0) * s).ceil() > sub_w as f64 && sub_x1 > sub_x0 {
                sub_x1 = sub_x1.next_down();
            }
            let mut sub_y0 = bg_rect.y1 - py1 as f64 / s;
            while ((sub_y1 - sub_y0) * s).ceil() > sub_h as f64 && sub_y0 < sub_y1 {
                sub_y0 = sub_y0.next_up();
            }
            let sub_render_rect = Rect::new(sub_x0, sub_y0, sub_x1, sub_y1);

            let clips = self.snapshot_clip_stack(false);
            let mut rendered =
                self.render_pass1_coverage(group_cmds, &clips, sub_render_rect, bg_scale)?;
            if rendered.data.as_chunks::<4>().0.iter().all(|px| px[3] == 0) {
                return Ok(());
            }

            let mut to_promote: Vec<usize> = Vec::new();
            if self.native_only_texts.is_active() {
                let preserves_black = group_preserves_opaque_black(group_cmds, self.font_cache);
                let preserves_white = group_preserves_opaque_white(group_cmds, self.font_cache);
                let mut overlap_err: Option<zpdf_core::Error> = None;
                self.native_only_texts.visit_overlapping(
                    Some(sub_render_rect),
                    &mut self.stats,
                    |rec_idx, rec| {
                        if overlap_err.is_none()
                            && pass1_touches_rect(&rendered, sub_render_rect, bg_scale, rec.bounds)
                        {
                            let promotable = rec.occluded_regions.is_empty()
                                && ((rec.is_opaque_black && preserves_black)
                                    || (rec.is_opaque_white && preserves_white));
                            if promotable {
                                to_promote.push(rec_idx);
                            } else {
                                overlap_err = Some(zpdf_core::Error::StreamDecode(
                                    "PPTX backdrop export overlaps native-only text without font outlines and cannot render the overlap faithfully".into(),
                                ));
                            }
                        }
                        true
                    },
                );
                if let Some(err) = overlap_err {
                    return Err(err);
                }
            }

            // Advance the full-page backdrop incrementally up to `end_idx`,
            // charging only active execute time against its render budget and
            // surfacing any budget timeout as an explicit export error.
            let surf_copy = {
                let full = self
                    .backdrop_mgr
                    .full_page
                    .as_mut()
                    .expect("initialized above");
                full.advance_to(all_commands, end_idx + 1, &mut self.stats)?;
                full.surface.renderer.current_surface()
            };

            composite_coverage_with_backdrop(
                &mut rendered,
                surf_copy,
                px0,
                py0,
                px1,
                py1,
                sub_w,
                sub_h,
            );

            let png_data = rgba_to_png(rendered.width, rendered.height, &rendered.data)?;
            let img_x1 = sub_x1.min(self.page_rect.x1);
            let img_y0 = sub_y0.max(self.page_rect.y0);
            let width_ppt = (img_x1 - sub_x0).max(0.0);
            let height_ppt = (sub_y1 - img_y0).max(0.0);
            self.image_counter += 1;
            let image_id = format!("image{}", self.image_counter);
            let (x_ppt, y_ppt) = self.pdf_to_ppt_coords(sub_x0, sub_y1);
            self.elements.push(SlideElement::Image {
                x_emu: pt_to_emu(x_ppt),
                y_emu: pt_to_emu(y_ppt),
                width_emu: pt_to_emu(width_ppt),
                height_emu: pt_to_emu(height_ppt),
                image_data: png_data,
                image_id,
            });
            self.promote_native_only_texts(&mut to_promote);
            self.stats.backdrop_groups_completed += 1;
            #[cfg(test)]
            if self
                .expire_backdrop_after_groups
                .is_some_and(|n| self.stats.backdrop_groups_completed >= n)
            {
                self.backdrop_mgr.expire_all_budgets();
            }
            return Ok(());
        }

        // Honor the raster dimension cap even on huge sheets (no floor that
        // could overshoot 2048 px or trigger an unexpected pixel-limit downscale).
        let scale = crop_scale;

        // Rebase any reused soft mask's `offset` into its commands before
        // rendering into the cropped target; otherwise `CpuRenderer` paints the
        // mask at its original position (potentially outside `crop_rect`) before
        // shifting the pixel plane. When `crop_w * scale` or `crop_h * scale`
        // leaves a fractional final pixel (e.g. a 16,384 × 12 pt crop at scale
        // 1/8 → 2048 × 2 px), fit `crop_rect` exactly onto the integer pixel
        // grid so the exported image stays within `crop_w × crop_h` without
        // enlarging past the crop or introducing fractional-edge alpha loss.
        let expected_w = (crop_w * scale as f64).ceil().max(1.0) as u32;
        let expected_h = (crop_h * scale as f64).ceil().max(1.0) as u32;
        let aligned_w = (expected_w as f64 - crop_w * scale as f64).abs() <= 1e-6;
        let aligned_h = (expected_h as f64 - crop_h * scale as f64).abs() <= 1e-6;
        let render_x1 = if aligned_w {
            crop_rect.x1
        } else {
            let mut x1 = crop_rect.x0 + expected_w as f64 / scale as f64;
            while ((x1 - crop_rect.x0) * scale as f64).ceil() > expected_w as f64
                && x1 > crop_rect.x0
            {
                x1 = x1.next_down();
            }
            x1
        };
        let render_y0 = if aligned_h {
            crop_rect.y0
        } else {
            let mut y0 = crop_rect.y1 - expected_h as f64 / scale as f64;
            while ((crop_rect.y1 - y0) * scale as f64).ceil() > expected_h as f64
                && y0 < crop_rect.y1
            {
                y0 = y0.next_up();
            }
            y0
        };
        let render_rect = Rect::new(crop_rect.x0, render_y0, render_x1, crop_rect.y1);
        let sx = if aligned_w {
            1.0
        } else {
            render_rect.width() / crop_w
        };
        let sy = if aligned_h {
            1.0
        } else {
            render_rect.height() / crop_h
        };

        let mut rebaser = SoftMaskRebaser::new(crop_rect.x0, crop_rect.y1, sx, sy, scale);
        if rebaser.is_unit_scale() {
            rebaser = rebaser.with_target_rect(render_rect);
        }

        let mut renderer = zpdf_render_cpu::CpuRenderer::new()
            .with_fonts(self.font_cache)
            .with_images(self.image_cache)
            .with_render_budget(self.render_budget);
        renderer
            .begin_page(&PageRenderInfo {
                page_rect: render_rect,
                scale,
                background: zpdf_display_list::Color::rgba(0.0, 0.0, 0.0, 0.0),
            })
            .map_err(|e| zpdf_core::Error::StreamDecode(format!("PPTX mask render: {e}")))?;
        for &clip in &self.clip_stack {
            let prepared = rebaser.prepare_command(clip);
            let exec_res = renderer
                .execute(prepared.as_ref())
                .map_err(|e| zpdf_core::Error::StreamDecode(format!("PPTX mask render: {e}")));
            if rebaser.take_needs_cpu_mask_cache_invalidation() {
                renderer.clear_soft_mask_cache();
            }
            drop(prepared);
            exec_res?;
            self.stats.pass1_command_visits += 1;
        }
        // Stream commands one by one (prepare -> execute -> invalidate cache if
        // transient -> drop) so Forms containing tiled patterns never retain all
        // translated masks alive simultaneously.
        for cmd in group_cmds {
            let prepared = rebaser.prepare_command(cmd);
            let exec_res = renderer
                .execute(prepared.as_ref())
                .map_err(|e| zpdf_core::Error::StreamDecode(format!("PPTX mask render: {e}")));
            if rebaser.take_needs_cpu_mask_cache_invalidation() {
                renderer.clear_soft_mask_cache();
                self.stats.flatten_cpu_mask_cache_invalidations += 1;
            }
            drop(prepared);
            exec_res?;
            self.stats.pass1_command_visits += 1;
            self.stats.peak_flatten_rebaser_mask_entries = self
                .stats
                .peak_flatten_rebaser_mask_entries
                .max(rebaser.translated_cache.len());
            self.stats.peak_flatten_rebaser_mask_bytes = self
                .stats
                .peak_flatten_rebaser_mask_bytes
                .max(rebaser.retained_bytes);
        }
        for _ in 0..self.clip_stack.len() {
            renderer
                .execute(&RenderCommand::PopClip)
                .map_err(|e| zpdf_core::Error::StreamDecode(format!("PPTX mask render: {e}")))?;
        }
        if renderer.is_over_budget() {
            return Err(zpdf_core::Error::StreamDecode(
                "PPTX mask render exceeded time budget".into(),
            ));
        }
        let mut rendered = renderer
            .end_page()
            .map_err(|e| zpdf_core::Error::StreamDecode(format!("PPTX mask render: {e}")))?;

        if rendered.data.as_chunks::<4>().0.iter().all(|px| px[3] == 0) {
            return Ok(());
        }

        // `CpuRenderer` produces premultiplied RGBA8; PNG expects straight RGBA8.
        unpremultiply_rgba(&mut rendered.data);
        let png_data = rgba_to_png(rendered.width, rendered.height, &rendered.data)?;

        self.image_counter += 1;
        let image_id = format!("image{}", self.image_counter);
        let (x_ppt, y_ppt) = self.pdf_to_ppt_coords(crop_rect.x0, crop_rect.y1);
        let (width_ppt, height_ppt) = (crop_w, crop_h);
        self.elements.push(SlideElement::Image {
            x_emu: pt_to_emu(x_ppt),
            y_emu: pt_to_emu(y_ppt),
            width_emu: pt_to_emu(width_ppt),
            height_emu: pt_to_emu(height_ppt),
            image_data: png_data,
            image_id,
        });
        self.note_paint_over_native_texts(Some(crop_rect), None, false, false);
        Ok(())
    }

    fn convert_command(
        &mut self,
        all_commands: &[RenderCommand],
        cmd_idx: usize,
        in_artwork_range: bool,
    ) -> Result<()> {
        let cmd = &all_commands[cmd_idx];
        match cmd {
            RenderCommand::DrawGlyphRun(glyph_run) => {
                let effective_alpha = effective_paint_alpha(&glyph_run.paint, glyph_run.alpha);
                if effective_alpha <= 0.0 || glyph_run.overprint.is_some_and(|op| op.active == 0) {
                    return Ok(());
                }
                let has_outlines = self
                    .font_cache
                    .get(glyph_run.font_id)
                    .is_some_and(|f| f.has_font_data());
                let is_tiling = matches!(glyph_run.paint, Paint::Tiling(_));
                let overprint_backdrop = command_has_renderable_overprint(cmd, self.font_cache);
                let tiling_backdrop = command_uses_tiling_backdrop(cmd, self.font_cache);
                if has_outlines && (is_tiling || in_artwork_range || overprint_backdrop) {
                    let backdrop = (overprint_backdrop || tiling_backdrop).then_some((
                        all_commands,
                        cmd_idx,
                        cmd_idx,
                    ));
                    self.flatten_commands_group(
                        &all_commands[cmd_idx..=cmd_idx],
                        None,
                        self.page_rect,
                        backdrop,
                    )?;
                } else if !has_outlines {
                    if in_artwork_range {
                        // Nested outline-less text inside unmapped Type 3 or pattern
                        // artwork must never leak into editable slide text: skip if
                        // clipped/off-page, otherwise report the missing outlines.
                        if !is_rect_visible_under_clips(
                            native_text_ink_bounds(glyph_run, self.font_cache),
                            &self.clip_stack,
                            self.page_rect,
                        ) {
                            return Ok(());
                        }
                        return Err(zpdf_core::Error::StreamDecode(
                            "PPTX glyph artwork contains nested text without font outlines and cannot be rasterized".into(),
                        ));
                    }
                    self.emit_native_only_glyph_run(all_commands, cmd_idx, glyph_run)?;
                } else {
                    if self.native_only_texts.is_active() {
                        let bounds = match self.clipped_command_bounds(cmd) {
                            CommandBounds::Empty => None,
                            CommandBounds::Known(r) => Some(Some(r)),
                            CommandBounds::Unknown => Some(None),
                        };
                        if let Some(b) = bounds {
                            self.note_paint_over_native_texts(
                                b,
                                None,
                                glyph_run_is_opaque_black(glyph_run),
                                glyph_run_is_opaque_white(glyph_run),
                            );
                        }
                    }
                    self.convert_glyph_run(glyph_run, false)?;
                }
            }
            RenderCommand::FillPath {
                path,
                rule,
                paint,
                alpha,
                overprint,
            } => {
                let effective_alpha = effective_paint_alpha(paint, *alpha);
                if effective_alpha <= 0.0 || overprint.is_some_and(|op| op.active == 0) {
                    return Ok(());
                }
                let is_tiling = matches!(paint, Paint::Tiling(_));
                let overprint_backdrop = overprint.is_some_and(|op| op.active != 0);
                let tiling_backdrop = command_uses_tiling_backdrop(cmd, self.font_cache);
                let clipped_artwork = in_artwork_range
                    && self.clip_stack_constrains_commands(std::slice::from_ref(cmd));
                let raster_artwork = is_tiling
                    || overprint_backdrop
                    || (in_artwork_range
                        && (effective_alpha < 1.0 - 1e-3
                            || artwork_fill_needs_raster(path, *rule, paint)));
                if clipped_artwork || raster_artwork {
                    let backdrop = (overprint_backdrop || tiling_backdrop).then_some((
                        all_commands,
                        cmd_idx,
                        cmd_idx,
                    ));
                    self.flatten_commands_group(
                        &all_commands[cmd_idx..=cmd_idx],
                        None,
                        self.page_rect,
                        backdrop,
                    )?;
                } else {
                    self.convert_fill_path(path, paint, effective_alpha, in_artwork_range)?;
                    if self.native_only_texts.is_active() {
                        match self.clipped_command_bounds(cmd) {
                            CommandBounds::Empty => {}
                            CommandBounds::Known(r) => {
                                let cover = opaque_normal_fill_cover_rect(cmd, &self.clip_stack);
                                let is_black = effective_alpha >= 1.0 - 1e-3
                                    && matches!(
                                        paint,
                                        zpdf_display_list::Paint::Solid(c)
                                            if color_to_rgb_u8(c) == (0, 0, 0)
                                    );
                                let is_white = effective_alpha >= 1.0 - 1e-3
                                    && matches!(
                                        paint,
                                        zpdf_display_list::Paint::Solid(c)
                                            if color_to_rgb_u8(c) == (255, 255, 255)
                                    );
                                self.note_paint_over_native_texts(
                                    Some(r),
                                    cover,
                                    is_black,
                                    is_white,
                                );
                            }
                            CommandBounds::Unknown => {
                                self.note_paint_over_native_texts(None, None, false, false);
                            }
                        }
                    }
                }
            }
            RenderCommand::StrokePath {
                path,
                style,
                paint,
                alpha,
                overprint,
            } => {
                let effective_alpha = effective_paint_alpha(paint, *alpha);
                if effective_alpha <= 0.0 || overprint.is_some_and(|op| op.active == 0) {
                    return Ok(());
                }
                let is_tiling = matches!(paint, Paint::Tiling(_));
                let overprint_backdrop = overprint.is_some_and(|op| op.active != 0);
                let tiling_backdrop = command_uses_tiling_backdrop(cmd, self.font_cache);
                let clipped_artwork = in_artwork_range
                    && self.clip_stack_constrains_commands(std::slice::from_ref(cmd));
                let raster_artwork = is_tiling
                    || overprint_backdrop
                    || (in_artwork_range
                        && (effective_alpha < 1.0 - 1e-3
                            || artwork_stroke_needs_raster(path, style, paint)));
                if clipped_artwork || raster_artwork {
                    let backdrop = (overprint_backdrop || tiling_backdrop).then_some((
                        all_commands,
                        cmd_idx,
                        cmd_idx,
                    ));
                    self.flatten_commands_group(
                        &all_commands[cmd_idx..=cmd_idx],
                        None,
                        self.page_rect,
                        backdrop,
                    )?;
                } else {
                    self.convert_stroke_path(path, style, paint, effective_alpha)?;
                    if self.native_only_texts.is_active() {
                        match self.clipped_command_bounds(cmd) {
                            CommandBounds::Empty => {}
                            CommandBounds::Known(r) => {
                                self.note_paint_over_native_texts(Some(r), None, false, false);
                            }
                            CommandBounds::Unknown => {
                                self.note_paint_over_native_texts(None, None, false, false);
                            }
                        }
                    }
                }
            }
            RenderCommand::DrawImage(image_draw) => {
                if !image_draw.alpha.is_finite() || image_draw.alpha <= 0.0 {
                    return Ok(());
                }
                if !image_draw.transform.is_finite() {
                    return Err(zpdf_core::Error::StreamDecode(
                        "non-finite PPTX image transform".into(),
                    ));
                }
                if (in_artwork_range
                    && self.clip_stack_constrains_commands(std::slice::from_ref(cmd)))
                    || matrix_has_shear(&image_draw.transform)
                {
                    let image = self
                        .image_cache
                        .get(image_draw.image_id)
                        .ok_or_else(|| zpdf_core::Error::StreamDecode("Image not found".into()))?;
                    validate_rgba_buffer(image.width, image.height, &image.data)?;
                    self.flatten_commands_group(
                        std::slice::from_ref(cmd),
                        None,
                        self.page_rect,
                        None,
                    )?;
                } else {
                    self.convert_image(image_draw)?;
                    if self.native_only_texts.is_active() {
                        match self.clipped_command_bounds(cmd) {
                            CommandBounds::Empty => {}
                            CommandBounds::Known(r) => {
                                self.note_paint_over_native_texts(Some(r), None, false, false);
                            }
                            CommandBounds::Unknown => {
                                self.note_paint_over_native_texts(None, None, false, false);
                            }
                        }
                    }
                }
            }
            RenderCommand::DrawInlineImage(image_draw) => {
                if !image_draw.alpha.is_finite() || image_draw.alpha <= 0.0 {
                    return Ok(());
                }
                if !image_draw.transform.is_finite() {
                    return Err(zpdf_core::Error::StreamDecode(
                        "non-finite PPTX image transform".into(),
                    ));
                }
                validate_rgba_buffer(
                    image_draw.image.width,
                    image_draw.image.height,
                    &image_draw.image.data,
                )?;
                if (in_artwork_range
                    && self.clip_stack_constrains_commands(std::slice::from_ref(cmd)))
                    || matrix_has_shear(&image_draw.transform)
                {
                    self.flatten_commands_group(
                        std::slice::from_ref(cmd),
                        None,
                        self.page_rect,
                        None,
                    )?;
                } else {
                    self.convert_inline_image(image_draw)?;
                    if self.native_only_texts.is_active() {
                        match self.clipped_command_bounds(cmd) {
                            CommandBounds::Empty => {}
                            CommandBounds::Known(r) => {
                                self.note_paint_over_native_texts(Some(r), None, false, false);
                            }
                            CommandBounds::Unknown => {
                                self.note_paint_over_native_texts(None, None, false, false);
                            }
                        }
                    }
                }
            }
            // Skip clip/blend commands - we're extracting content only
            _ => {}
        }
        Ok(())
    }

    fn prepare_glyph_run(
        &self,
        glyph_run: &zpdf_display_list::GlyphRun,
        require_verified_all: bool,
        resolved_paint_override: Option<((u8, u8, u8), f32)>,
    ) -> Result<Option<SlideElement>> {
        use zpdf_display_list::Paint;

        // Extract text from glyphs using the logical PDF font (following any
        // admitted drawing-only companion/fallback face back to its original
        // Type 0 / simple PDF font so `/ToUnicode` and `/Encoding` stay intact).
        let font = self
            .font_cache
            .logical_font(glyph_run.font_id)
            .ok_or_else(|| zpdf_core::Error::StreamDecode("Font not found".into()))?;

        if glyph_run.glyphs.is_empty() {
            return Ok(None);
        }
        let effective_alpha = resolved_paint_override
            .map(|(_, a)| a)
            .unwrap_or_else(|| effective_paint_alpha(&glyph_run.paint, glyph_run.alpha));
        if effective_alpha <= 0.0
            || (require_verified_all
                && (effective_alpha < 1.0 - 1e-3 || glyph_run.overprint.is_some()))
        {
            return Ok(None);
        }

        let mut text = String::new();
        let mut min_x = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_y = f32::NEG_INFINITY;

        for glyph in &glyph_run.glyphs {
            if require_verified_all {
                // When deciding whether to replace expanded CharProc geometry
                // with an editable TextBox, every glyph in the range must have
                // an explicit Unicode mapping (`/ToUnicode` or a recognized
                // glyph name for Type 3 — never raw printable-ASCII fallback).
                if font.is_type3() {
                    if !font.append_verified_type3_unicode(glyph.code, glyph.code_len, &mut text) {
                        return Ok(None);
                    }
                } else {
                    let before = text.len();
                    font.append_decoded_source_code(glyph.code, glyph.code_len, &mut text);
                    if text.len() == before {
                        return Ok(None);
                    }
                }
            } else {
                // Decode each glyph's packed source bytes and byte width through
                // the logical PDF font's own text decoder (`/ToUnicode`, then
                // composite `/Encoding` CMap or simple encoding / glyph names).
                font.append_decoded_source_code(glyph.code, glyph.code_len, &mut text);
            }

            // Track bounding box
            min_x = min_x.min(glyph.x);
            max_x = max_x.max(glyph.x + glyph.advance);
            min_y = min_y.min(glyph.y);
            max_y = max_y.max(glyph.y + glyph_run.font_size);
        }

        if text.is_empty() {
            // If we couldn't extract text, skip this glyph run
            return Ok(None);
        }

        // Extract color from paint (or resolved overprint override)
        let color_rgb = resolved_paint_override
            .map(|(rgb, _)| rgb)
            .unwrap_or_else(|| {
                match &glyph_run.paint {
                    Paint::Solid(c) => color_to_rgb_u8(c),
                    _ => (0, 0, 0), // Default to black for patterns/shadings
                }
            });

        // Convert PDF coordinates (bottom-left origin) to PowerPoint (top-left origin)
        let matrix = &glyph_run.transform;
        if !matrix.is_finite() {
            return Err(zpdf_core::Error::StreamDecode(
                "non-finite PPTX text transform".into(),
            ));
        }
        let scale_x = matrix.a.hypot(matrix.b);
        let scale_y = matrix.c.hypot(matrix.d);
        if scale_x == 0.0 || scale_y == 0.0 {
            return Ok(None);
        }
        let center = zpdf_core::Point::new(
            (min_x as f64 + max_x as f64) / 2.0,
            (min_y as f64 + max_y as f64) / 2.0,
        )
        .transform(matrix);
        let (center_x, center_y) = self.pdf_to_ppt_coords(center.x, center.y);
        let width = (max_x - min_x) as f64 * scale_x;
        let height = (max_y - min_y) as f64 * scale_y;
        let (x_ppt, y_ppt) = (center_x - width / 2.0, center_y - height / 2.0);

        // Extract font family name from base_font
        let font_family = font
            .base_font
            .split_once('+')
            .filter(|(prefix, _)| {
                prefix.len() == 6 && prefix.bytes().all(|b| b.is_ascii_uppercase())
            })
            .map(|(_, name)| name)
            .unwrap_or(&font.base_font)
            .to_string();
        let style = font_family.to_ascii_lowercase();

        let element = SlideElement::TextBox {
            text,
            x_emu: pt_to_emu(x_ppt),
            y_emu: pt_to_emu(y_ppt),
            width_emu: pt_to_emu(width),
            height_emu: pt_to_emu(height),
            font_family,
            font_size_pt: glyph_run.font_size as f64 * scale_y,
            color_rgb,
            bold: style.contains("bold") || style.contains("demi"),
            italic: style.contains("italic") || style.contains("oblique"),
        };
        Ok(Some(with_opacity(
            transformed_element(element, matrix),
            effective_alpha,
        )))
    }

    fn convert_glyph_run(
        &mut self,
        glyph_run: &zpdf_display_list::GlyphRun,
        require_verified_all: bool,
    ) -> Result<bool> {
        self.convert_glyph_run_with_paint(glyph_run, require_verified_all, None)
    }

    fn convert_glyph_run_with_paint(
        &mut self,
        glyph_run: &zpdf_display_list::GlyphRun,
        require_verified_all: bool,
        resolved_paint_override: Option<((u8, u8, u8), f32)>,
    ) -> Result<bool> {
        let Some(element) =
            self.prepare_glyph_run(glyph_run, require_verified_all, resolved_paint_override)?
        else {
            return Ok(false);
        };
        self.elements.push(element);
        Ok(true)
    }

    fn convert_fill_path(
        &mut self,
        path: &zpdf_display_list::Path,
        paint: &zpdf_display_list::Paint,
        effective_alpha: f32,
        in_artwork_range: bool,
    ) -> Result<()> {
        use zpdf_display_list::Paint;

        let fill_rgb = match paint {
            Paint::Solid(c) => Some(color_to_rgb_u8(c)),
            _ => None,
        };

        // Try to recognize simple shapes (glyph artwork requires an exact
        // four-corner axis-aligned rectangle so explicitly closed triangles
        // or sheared/rotated quads never expand to their bounding box).
        if self.options.recognize_rectangles {
            let recognized = if in_artwork_range {
                exact_axis_aligned_rect(path)
            } else {
                shape_recognizer::recognize_rectangle(path)
            };
            if let Some(rect) = recognized {
                let (x_ppt, y_ppt) = self.pdf_to_ppt_coords(rect.x0, rect.y1);
                let element = SlideElement::Rectangle {
                    x_emu: pt_to_emu(x_ppt),
                    y_emu: pt_to_emu(y_ppt),
                    width_emu: pt_to_emu(rect.width()),
                    height_emu: pt_to_emu(rect.height()),
                    fill_rgb,
                    stroke_rgb: None,
                    stroke_width_pt: 0.0,
                };
                self.elements.push(with_opacity(element, effective_alpha));
                return Ok(());
            }
        }

        if !in_artwork_range && self.options.recognize_ellipses {
            if let Some(rect) = shape_recognizer::recognize_ellipse(path) {
                let (x_ppt, y_ppt) = self.pdf_to_ppt_coords(rect.x0, rect.y1);
                let element = SlideElement::Ellipse {
                    x_emu: pt_to_emu(x_ppt),
                    y_emu: pt_to_emu(y_ppt),
                    width_emu: pt_to_emu(rect.width()),
                    height_emu: pt_to_emu(rect.height()),
                    fill_rgb,
                    stroke_rgb: None,
                    stroke_width_pt: 0.0,
                };
                self.elements.push(with_opacity(element, effective_alpha));
                return Ok(());
            }
        }

        // Fall back to freeform path
        let path_data = self.path_to_svg(path);
        let element = SlideElement::FreeformPath {
            path_data,
            fill_rgb,
            stroke_rgb: None,
            stroke_width_pt: 0.0,
        };
        self.elements.push(with_opacity(element, effective_alpha));

        Ok(())
    }

    fn convert_stroke_path(
        &mut self,
        path: &zpdf_display_list::Path,
        style: &zpdf_display_list::StrokeStyle,
        paint: &zpdf_display_list::Paint,
        effective_alpha: f32,
    ) -> Result<()> {
        use zpdf_display_list::Paint;

        let stroke_rgb = match paint {
            Paint::Solid(c) => Some(color_to_rgb_u8(c)),
            _ => None,
        };

        // Check if it's a simple line
        if let Some((p1, p2)) = shape_recognizer::recognize_line(path) {
            let (x1_ppt, y1_ppt) = self.pdf_to_ppt_coords(p1.x, p1.y);
            let (x2_ppt, y2_ppt) = self.pdf_to_ppt_coords(p2.x, p2.y);
            let element = SlideElement::Line {
                x1_emu: pt_to_emu(x1_ppt),
                y1_emu: pt_to_emu(y1_ppt),
                x2_emu: pt_to_emu(x2_ppt),
                y2_emu: pt_to_emu(y2_ppt),
                stroke_rgb: stroke_rgb.unwrap_or((0, 0, 0)),
                stroke_width_pt: style.width as f64,
            };
            self.elements.push(with_opacity(element, effective_alpha));
            return Ok(());
        }

        // Complex stroked path
        let path_data = self.path_to_svg(path);
        let element = SlideElement::FreeformPath {
            path_data,
            fill_rgb: None,
            stroke_rgb,
            stroke_width_pt: style.width as f64,
        };
        self.elements.push(with_opacity(element, effective_alpha));

        Ok(())
    }

    fn convert_image(&mut self, image_draw: &zpdf_display_list::ImageDraw) -> Result<()> {
        let image = self
            .image_cache
            .get(image_draw.image_id)
            .ok_or_else(|| zpdf_core::Error::StreamDecode("Image not found".into()))?;
        self.convert_rgba_image(
            image.width,
            image.height,
            &image.data,
            image.premultiplied,
            image_draw.alpha,
            &image_draw.transform,
        )
    }

    fn convert_inline_image(
        &mut self,
        image_draw: &zpdf_display_list::InlineImageDraw,
    ) -> Result<()> {
        self.convert_rgba_image(
            image_draw.image.width,
            image_draw.image.height,
            &image_draw.image.data,
            image_draw.image.premultiplied,
            image_draw.alpha,
            &image_draw.transform,
        )
    }

    fn convert_rgba_image(
        &mut self,
        img_width: u32,
        img_height: u32,
        data: &[u8],
        premultiplied: bool,
        alpha: f32,
        mat: &zpdf_core::Matrix,
    ) -> Result<()> {
        if !mat.is_finite() {
            return Err(zpdf_core::Error::StreamDecode(
                "non-finite PPTX image transform".into(),
            ));
        }
        validate_rgba_buffer(img_width, img_height, data)?;

        // Image transform: unit square [0,1]×[0,1] → page space
        let width = (mat.a * mat.a + mat.b * mat.b).sqrt();
        let height = (mat.c * mat.c + mat.d * mat.d).sqrt();

        let center = zpdf_core::Point::new(0.5, 0.5).transform(mat);
        let (cx, cy) = self.pdf_to_ppt_coords(center.x, center.y);
        let (x_ppt, y_ppt) = (cx - width / 2.0, cy - height / 2.0);

        let alpha = if alpha.is_finite() {
            alpha.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let scale_alpha = (alpha - 1.0).abs() > f32::EPSILON;
        let needs_unpremul = premultiplied
            && data
                .as_chunks::<4>()
                .0
                .iter()
                .any(|px| px[3] > 0 && px[3] < 255);

        // Convert image to PNG, preserving command opacity and straight RGBA.
        let png_data = if scale_alpha || needs_unpremul {
            let mut rgba = data.to_vec();
            if needs_unpremul {
                unpremultiply_rgba(&mut rgba);
            }
            if scale_alpha {
                for px in rgba.as_chunks_mut::<4>().0 {
                    px[3] = ((px[3] as f32 * alpha).round() as u32).min(255) as u8;
                }
            }
            rgba_to_png(img_width, img_height, &rgba)?
        } else {
            rgba_to_png(img_width, img_height, data)?
        };

        self.image_counter += 1;
        let image_id = format!("image{}", self.image_counter);

        let element = SlideElement::Image {
            x_emu: pt_to_emu(x_ppt),
            y_emu: pt_to_emu(y_ppt),
            width_emu: pt_to_emu(width),
            height_emu: pt_to_emu(height),
            image_data: png_data,
            image_id,
        };
        self.elements.push(transformed_element(element, mat));

        Ok(())
    }

    /// Convert PDF coordinates (bottom-left origin, Y+ up) to PowerPoint (top-left origin, Y+ down)
    fn pdf_to_ppt_coords(&self, x: f64, y: f64) -> (f64, f64) {
        let x_ppt = x - self.page_rect.x0;
        let y_ppt = self.page_rect.y1 - y;
        (x_ppt, y_ppt)
    }

    /// Convert a display-list Path to SVG path data
    fn path_to_svg(&self, path: &zpdf_display_list::Path) -> String {
        use zpdf_display_list::PathElement;

        let mut svg = String::new();
        for elem in path {
            match elem {
                PathElement::MoveTo(p) => {
                    let (x, y) = self.pdf_to_ppt_coords(p.x, p.y);
                    svg.push_str(&format!("M {:.2} {:.2} ", x, y));
                }
                PathElement::LineTo(p) => {
                    let (x, y) = self.pdf_to_ppt_coords(p.x, p.y);
                    svg.push_str(&format!("L {:.2} {:.2} ", x, y));
                }
                PathElement::CurveTo(c1, c2, end) => {
                    let (x1, y1) = self.pdf_to_ppt_coords(c1.x, c1.y);
                    let (x2, y2) = self.pdf_to_ppt_coords(c2.x, c2.y);
                    let (x, y) = self.pdf_to_ppt_coords(end.x, end.y);
                    svg.push_str(&format!(
                        "C {:.2} {:.2} {:.2} {:.2} {:.2} {:.2} ",
                        x1, y1, x2, y2, x, y
                    ));
                }
                PathElement::Close => {
                    svg.push_str("Z ");
                }
            }
        }
        svg
    }
}

/// Convert points to EMUs (English Metric Units).
/// 1 inch = 72 points = 914400 EMUs
fn pt_to_emu(pt: f64) -> i64 {
    (pt * 12700.0) as i64
}

#[derive(Debug, Clone, Copy)]
enum CommandBounds {
    Empty,
    Known(Rect),
    Unknown,
}

fn compute_commands_bounds(
    commands: &[RenderCommand],
    offset: (f32, f32),
    font_cache: &FontCache,
) -> CommandBounds {
    let dx = offset.0 as f64;
    let dy = offset.1 as f64;
    if !dx.is_finite() || !dy.is_finite() {
        return CommandBounds::Unknown;
    }
    let mut x0 = f64::INFINITY;
    let mut y0 = f64::INFINITY;
    let mut x1 = f64::NEG_INFINITY;
    let mut y1 = f64::NEG_INFINITY;
    let mut saw_paint = false;
    let mut include_pt = |x: f64, y: f64| -> bool {
        let sx = x + dx;
        let sy = y + dy;
        if !(sx.is_finite() && sy.is_finite()) {
            return false;
        }
        x0 = x0.min(sx);
        y0 = y0.min(sy);
        x1 = x1.max(sx);
        y1 = y1.max(sy);
        true
    };
    for cmd in commands {
        match cmd {
            RenderCommand::DrawImage(d) => {
                saw_paint = true;
                for &(u, v) in &[(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
                    let p = zpdf_core::Point::new(u, v).transform(&d.transform);
                    if !include_pt(p.x, p.y) {
                        return CommandBounds::Unknown;
                    }
                }
            }
            RenderCommand::DrawInlineImage(d) => {
                saw_paint = true;
                for &(u, v) in &[(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
                    let p = zpdf_core::Point::new(u, v).transform(&d.transform);
                    if !include_pt(p.x, p.y) {
                        return CommandBounds::Unknown;
                    }
                }
            }
            RenderCommand::FillPath { path, .. } => {
                if path.is_empty() {
                    continue;
                }
                saw_paint = true;
                for p in path.points() {
                    if !include_pt(p.x, p.y) {
                        return CommandBounds::Unknown;
                    }
                }
            }
            RenderCommand::StrokePath { path, style, .. } => {
                if path.is_empty() {
                    continue;
                }
                saw_paint = true;
                let Some(grow) = stroke_style_grow(style) else {
                    return CommandBounds::Unknown;
                };
                for p in path.points() {
                    if !(include_pt(p.x - grow, p.y - grow) && include_pt(p.x + grow, p.y + grow)) {
                        return CommandBounds::Unknown;
                    }
                }
            }
            RenderCommand::DrawGlyphRun(run) => {
                if run.glyphs.is_empty() {
                    continue;
                }
                let Some(font) = font_cache.get(run.font_id) else {
                    return CommandBounds::Unknown;
                };
                if !font.has_font_data() {
                    continue;
                }
                if font.is_type3() {
                    return CommandBounds::Unknown;
                }
                let upem = font.units_per_em as f32;
                if !upem.is_finite()
                    || upem <= 0.0
                    || !run.font_size.is_finite()
                    || !run.h_scale.is_finite()
                    || !run.transform.is_finite()
                {
                    return CommandBounds::Unknown;
                }
                for glyph in &run.glyphs {
                    let Some(outline) = font.glyph_outline(glyph.glyph_id) else {
                        continue;
                    };
                    if outline.commands.is_empty() {
                        continue;
                    }
                    saw_paint = true;
                    let mut include_outline = |gx: f64, gy: f64| -> bool {
                        let tx = (gx as f32 / upem * run.font_size * run.h_scale + glyph.x) as f64;
                        let ty = (gy as f32 / upem * run.font_size + glyph.y) as f64;
                        let page_x = run.transform.a * tx + run.transform.c * ty + run.transform.e;
                        let page_y = run.transform.b * tx + run.transform.d * ty + run.transform.f;
                        include_pt(page_x, page_y)
                    };
                    for oc in &outline.commands {
                        let ok = match *oc {
                            zpdf_font::OutlineCommand::MoveTo(x, y)
                            | zpdf_font::OutlineCommand::LineTo(x, y) => include_outline(x, y),
                            zpdf_font::OutlineCommand::QuadTo(x1, y1, x, y) => {
                                include_outline(x1, y1) && include_outline(x, y)
                            }
                            zpdf_font::OutlineCommand::CurveTo(x1, y1, x2, y2, x, y) => {
                                include_outline(x1, y1)
                                    && include_outline(x2, y2)
                                    && include_outline(x, y)
                            }
                            zpdf_font::OutlineCommand::Close => true,
                        };
                        if !ok {
                            return CommandBounds::Unknown;
                        }
                    }
                }
            }
            RenderCommand::PushClip { .. }
            | RenderCommand::PushClipStroke { .. }
            | RenderCommand::PopClip
            | RenderCommand::PushBlendGroup { .. }
            | RenderCommand::PopBlendGroup => {}
        }
    }
    if !saw_paint {
        return CommandBounds::Empty;
    }
    if x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite() && x1 >= x0 && y1 >= y0
    {
        CommandBounds::Known(Rect::new(x0, y0, x1, y1))
    } else {
        CommandBounds::Unknown
    }
}

fn stroke_style_grow(style: &zpdf_display_list::StrokeStyle) -> Option<f64> {
    let half = (style.width.max(0.0) as f64 * 0.5).max(0.5);
    let join_grow = match style.join {
        zpdf_display_list::LineJoin::Miter => half * style.miter_limit.max(1.0) as f64,
        _ => half,
    };
    let cap_grow = match style.cap {
        zpdf_display_list::LineCap::Square => half * std::f64::consts::SQRT_2,
        zpdf_display_list::LineCap::Round => half,
        zpdf_display_list::LineCap::Butt => 0.0,
    };
    let grow = join_grow.max(cap_grow);
    grow.is_finite().then_some(grow)
}

fn tighten_rect_to_clip(raw_bounds: Rect, clip_cmd: &RenderCommand) -> Option<Rect> {
    match clip_cmd {
        RenderCommand::PushClip { path, .. } => {
            if path.is_empty() {
                return None;
            }
            if let Some(clip_rect) = path_control_bounds(path) {
                let padded = Rect::new(
                    clip_rect.x0 - 1.0,
                    clip_rect.y0 - 1.0,
                    clip_rect.x1 + 1.0,
                    clip_rect.y1 + 1.0,
                );
                intersect_rect(raw_bounds, padded)
            } else {
                Some(raw_bounds)
            }
        }
        RenderCommand::PushClipStroke { path, style } => {
            if path.is_empty() {
                return None;
            }
            if let (Some(clip_rect), Some(grow)) =
                (path_control_bounds(path), stroke_style_grow(style))
            {
                let pad = grow + 1.0;
                let padded = Rect::new(
                    clip_rect.x0 - pad,
                    clip_rect.y0 - pad,
                    clip_rect.x1 + pad,
                    clip_rect.y1 + pad,
                );
                intersect_rect(raw_bounds, padded)
            } else {
                Some(raw_bounds)
            }
        }
        _ => Some(raw_bounds),
    }
}

/// Returns `Some(rect)` iff `path` is a single simple non-degenerate
/// axis-aligned rectangle (alternating horizontal and vertical edges across 4
/// corners, no curves or multiple subpaths). Because such a polygon has no
/// self-intersections, its interior is identical under NonZero and EvenOdd.
fn exact_axis_aligned_rect(path: &zpdf_display_list::Path) -> Option<Rect> {
    use zpdf_display_list::PathElement;

    let mut pts: Vec<zpdf_core::Point> = Vec::with_capacity(5);
    let mut saw_move = false;
    let mut saw_close = false;
    for elem in path.iter() {
        match elem {
            PathElement::MoveTo(p) => {
                if saw_move || !(p.x.is_finite() && p.y.is_finite()) {
                    return None;
                }
                saw_move = true;
                pts.push(p);
            }
            PathElement::LineTo(p) => {
                if !saw_move || saw_close || !(p.x.is_finite() && p.y.is_finite()) {
                    return None;
                }
                pts.push(p);
            }
            PathElement::Close => {
                if !saw_move || saw_close {
                    return None;
                }
                saw_close = true;
            }
            PathElement::CurveTo(..) => return None,
        }
    }
    if pts.len() == 5 && (pts[4].x - pts[0].x).abs() <= 1e-6 && (pts[4].y - pts[0].y).abs() <= 1e-6
    {
        pts.pop();
    }
    if pts.len() != 4 {
        return None;
    }
    let mut prev_horiz: Option<bool> = None;
    let mut x0 = f64::INFINITY;
    let mut y0 = f64::INFINITY;
    let mut x1 = f64::NEG_INFINITY;
    let mut y1 = f64::NEG_INFINITY;
    for i in 0..4 {
        let a = pts[i];
        let b = pts[(i + 1) % 4];
        let dx = (b.x - a.x).abs();
        let dy = (b.y - a.y).abs();
        let is_horiz = if dy <= 1e-6 && dx > 1e-6 {
            true
        } else if dx <= 1e-6 && dy > 1e-6 {
            false
        } else {
            return None;
        };
        if let Some(prev) = prev_horiz {
            if prev == is_horiz {
                return None;
            }
        }
        prev_horiz = Some(is_horiz);
        x0 = x0.min(a.x);
        y0 = y0.min(a.y);
        x1 = x1.max(a.x);
        y1 = y1.max(a.y);
    }
    (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, x1, y1))
}

fn glyph_run_box_bounds(run: &zpdf_display_list::GlyphRun) -> Option<Rect> {
    if run.glyphs.is_empty() || !run.transform.is_finite() || !run.font_size.is_finite() {
        return None;
    }
    let mut min_x = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for g in &run.glyphs {
        min_x = min_x.min(g.x);
        max_x = max_x.max(g.x + g.advance);
        min_y = min_y.min(g.y);
        max_y = max_y.max(g.y + run.font_size);
    }
    if !(min_x.is_finite() && max_x.is_finite() && min_y.is_finite() && max_y.is_finite()) {
        return None;
    }
    let mut x0 = f64::INFINITY;
    let mut y0 = f64::INFINITY;
    let mut x1 = f64::NEG_INFINITY;
    let mut y1 = f64::NEG_INFINITY;
    for (lx, ly) in [
        (min_x as f64, min_y as f64),
        (max_x as f64, min_y as f64),
        (min_x as f64, max_y as f64),
        (max_x as f64, max_y as f64),
    ] {
        let p = zpdf_core::Point::new(lx, ly).transform(&run.transform);
        if !(p.x.is_finite() && p.y.is_finite()) {
            return None;
        }
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    Some(Rect::new(x0, y0, x1, y1))
}

fn expanded_run_bounds(
    entry: &zpdf_display_list::ExpandedGlyphRun,
    commands: &[RenderCommand],
    font_cache: &FontCache,
) -> Option<Rect> {
    let text_box = glyph_run_box_bounds(&entry.run)?;
    let end = entry.cmd_end.min(commands.len());
    let start = entry.cmd_start.min(end);
    match compute_commands_bounds(&commands[start..end], (0.0, 0.0), font_cache) {
        CommandBounds::Empty => Some(text_box),
        CommandBounds::Known(r) => Some(Rect::new(
            r.x0.min(text_box.x0),
            r.y0.min(text_box.y0),
            r.x1.max(text_box.x1),
            r.y1.max(text_box.y1),
        )),
        CommandBounds::Unknown => None,
    }
}

fn type3_glyph_range_starting_at(display_list: &DisplayList, cmd_idx: usize) -> Option<usize> {
    let ranges = display_list.type3_glyph_ranges();
    ranges
        .binary_search_by_key(&cmd_idx, |&(start, _)| start)
        .ok()
        .map(|i| ranges[i].1)
}

/// Linear O(N) pre-pass marking each `PushBlendGroup` index that contains a
/// nested backdrop-sampling operation (non-Normal `PushBlendGroup` or
/// renderable partial-colorant `Overprint`). Used so an isolated Normal group
/// containing such an operation flattens as one isolated unit rather than
/// losing its inner backdrop when `current_surface()` is unavailable inside
/// the group, while outline-less metrics-only fonts remain native text.
fn compute_isolated_inner_non_normal_blends(
    commands: &[RenderCommand],
    font_cache: &FontCache,
    soft_mask_cache: &mut SoftMaskAnalysisCache,
    stats: &mut ConversionStats,
) -> Vec<bool> {
    let mut flags = vec![false; commands.len()];
    if !commands.iter().any(|c| {
        matches!(
            c,
            RenderCommand::PushBlendGroup {
                blend_mode,
                alpha,
                ..
            } if *blend_mode != zpdf_display_list::BlendMode::Normal
                && alpha.is_finite()
                && *alpha > 0.0
        ) || command_has_renderable_overprint(c, font_cache)
            || command_uses_tiling_backdrop(c, font_cache)
    }) {
        return flags;
    }
    let mut stack: Vec<(usize, bool, bool)> = Vec::new();
    let mut zero_alpha_depth = 0usize;
    for (idx, cmd) in commands.iter().enumerate() {
        match cmd {
            RenderCommand::PushBlendGroup {
                blend_mode,
                isolated,
                alpha,
                mask,
                ..
            } => {
                let group_alpha = if alpha.is_finite() {
                    alpha.clamp(0.0, 1.0)
                } else {
                    0.0
                };
                if zero_alpha_depth > 0
                    || group_alpha <= 0.0
                    || soft_mask_cache
                        .analyze(mask.as_ref(), font_cache, stats)
                        .is_proven_empty()
                {
                    zero_alpha_depth += 1;
                    continue;
                }
                let non_normal = *blend_mode != zpdf_display_list::BlendMode::Normal;
                if non_normal {
                    if let Some((_, _, has_inner)) = stack.last_mut() {
                        *has_inner = true;
                    }
                }
                // An isolated Normal group supplies its own backdrop and resolves
                // inner non-Normal blends and partial-colorant overprints itself,
                // so its internal dependency must not propagate to an outer Form.
                let isolates_inner = !non_normal && *isolated;
                stack.push((idx, isolates_inner, false));
            }
            RenderCommand::PopBlendGroup => {
                if zero_alpha_depth > 0 {
                    zero_alpha_depth -= 1;
                    continue;
                }
                if let Some((push_idx, isolates_inner, has_inner)) = stack.pop() {
                    flags[push_idx] = has_inner;
                    if has_inner && !isolates_inner {
                        if let Some((_, _, parent_inner)) = stack.last_mut() {
                            *parent_inner = true;
                        }
                    }
                }
            }
            other
                if zero_alpha_depth == 0
                    && (command_has_renderable_overprint(other, font_cache)
                        || command_uses_tiling_backdrop(other, font_cache)) =>
            {
                if let Some((_, _, has_inner)) = stack.last_mut() {
                    *has_inner = true;
                }
            }
            _ => {}
        }
    }
    flags
}

fn is_paint_render_command(cmd: &RenderCommand) -> bool {
    matches!(
        cmd,
        RenderCommand::FillPath { .. }
            | RenderCommand::StrokePath { .. }
            | RenderCommand::DrawGlyphRun(_)
            | RenderCommand::DrawImage(_)
            | RenderCommand::DrawInlineImage(_)
    )
}

#[allow(clippy::too_many_arguments)]
fn composite_coverage_with_backdrop(
    rendered: &mut zpdf_render_cpu::RenderedPage,
    surf_copy: Option<(u32, u32, &[u8])>,
    px0: u32,
    py0: u32,
    px1: u32,
    py1: u32,
    sub_w: u32,
    sub_h: u32,
) {
    if let Some((surf_w, surf_h, surf_data)) = surf_copy {
        if px1 <= surf_w && py1 <= surf_h && rendered.width == sub_w && rendered.height == sub_h {
            let sw = sub_w as usize;
            let bw = surf_w as usize;
            for y in 0..sub_h as usize {
                let dst_row = y * sw * 4;
                let src_row = (py0 as usize + y) * bw * 4 + px0 as usize * 4;
                for x in 0..sw {
                    let d = dst_row + x * 4;
                    if rendered.data[d + 3] == 0 {
                        rendered.data[d..d + 4].copy_from_slice(&[0, 0, 0, 0]);
                    } else {
                        let b = src_row + x * 4;
                        rendered.data[d..d + 4].copy_from_slice(&[
                            surf_data[b],
                            surf_data[b + 1],
                            surf_data[b + 2],
                            255,
                        ]);
                    }
                }
            }
            return;
        }
    }
    unpremultiply_rgba(&mut rendered.data);
}

/// Spatial index of top-level backdrop-sampling non-Normal blend groups on a
/// page, used so expanded Type 3 glyph runs overlapped by a later non-Normal
/// blend keep consistent CharProc artwork instead of mixing editable text with
/// CharProc pixels in the blend patch.
struct NonNormalBlendIndex {
    origin_x: f64,
    origin_y: f64,
    /// Largest start command index of any genuinely unbounded (unclosed) blend group.
    max_unbounded_start: Option<usize>,
    /// Finite blend groups spanning > 16 grid cells, retaining their exact bounds.
    large_entries: Vec<(usize, Rect)>,
    /// Bounded backdrop-sampling blend groups bucketed onto the 512pt grid.
    buckets: std::collections::HashMap<(i32, i32), Vec<(usize, Rect)>>,
}

impl NonNormalBlendIndex {
    fn empty(page_rect: Rect) -> Self {
        Self {
            origin_x: page_rect.x0,
            origin_y: page_rect.y0,
            max_unbounded_start: None,
            large_entries: Vec::new(),
            buckets: std::collections::HashMap::new(),
        }
    }

    fn build(
        commands: &[RenderCommand],
        page_rect: Rect,
        font_cache: &FontCache,
        soft_mask_cache: &mut SoftMaskAnalysisCache,
        stats: &mut ConversionStats,
        enabled: bool,
    ) -> Self {
        let mut index = Self::empty(page_rect);
        if !enabled {
            return index;
        }
        let mut isolated_normal_depth = 0usize;
        // Stack of cumulative active caller clip rects (None means clipped empty).
        let mut clip_stack: Vec<Option<Rect>> = Vec::new();
        let mut idx = 0usize;
        while idx < commands.len() {
            match &commands[idx] {
                RenderCommand::PushClip { .. } | RenderCommand::PushClipStroke { .. } => {
                    let cur = clip_stack.last().copied().unwrap_or(Some(page_rect));
                    let next = cur.and_then(|r| tighten_rect_to_clip(r, &commands[idx]));
                    clip_stack.push(next);
                }
                RenderCommand::PopClip => {
                    clip_stack.pop();
                }
                RenderCommand::PushBlendGroup {
                    blend_mode,
                    isolated,
                    knockout,
                    alpha,
                    mask,
                    bounds,
                } => {
                    let group_alpha = if alpha.is_finite() {
                        alpha.clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    if isolated_normal_depth == 0 {
                        if *blend_mode != zpdf_display_list::BlendMode::Normal {
                            let active_clip = clip_stack.last().copied().unwrap_or(Some(page_rect));
                            if let Some(end_idx) =
                                SlideConverter::find_matching_pop_blend(commands, idx)
                            {
                                if group_alpha > 0.0 {
                                    if let Some(clip_rect) = active_clip {
                                        let group_slice = &commands[idx..=end_idx];
                                        let fallback =
                                            intersect_rect(*bounds, clip_rect).unwrap_or(clip_rect);
                                        let draw_rect = match compute_commands_bounds(
                                            group_slice,
                                            (0.0, 0.0),
                                            font_cache,
                                        ) {
                                            CommandBounds::Empty => None,
                                            CommandBounds::Known(r) => Some(Rect::new(
                                                r.x0 - 1.0,
                                                r.y0 - 1.0,
                                                r.x1 + 1.0,
                                                r.y1 + 1.0,
                                            )),
                                            CommandBounds::Unknown => Some(fallback),
                                        };
                                        let effective = draw_rect.and_then(|dr| {
                                            match soft_mask_cache
                                                .analyze(mask.as_ref(), font_cache, stats)
                                                .potential_bounds()
                                            {
                                                RenderableMaskBounds::Empty => None,
                                                RenderableMaskBounds::Bounded(padded) => {
                                                    intersect_rect(dr, padded)
                                                }
                                                RenderableMaskBounds::Unbounded => Some(dr),
                                            }
                                        });
                                        let mut effective_opt =
                                            effective.and_then(|r| intersect_rect(r, clip_rect));
                                        if let Some(eff) = effective_opt {
                                            if let Some(first_inner) = group_slice.get(1) {
                                                if matches!(
                                                    first_inner,
                                                    RenderCommand::PushClip { .. }
                                                        | RenderCommand::PushClipStroke { .. }
                                                ) {
                                                    let covers_all_paint =
                                                        SlideConverter::find_matching_pop_clip(
                                                            group_slice,
                                                            1,
                                                        )
                                                        .is_some_and(|pop_rel| {
                                                            !group_slice[pop_rel + 1..]
                                                                .iter()
                                                                .any(is_paint_render_command)
                                                        });
                                                    if covers_all_paint {
                                                        effective_opt =
                                                            tighten_rect_to_clip(eff, first_inner);
                                                    }
                                                }
                                            }
                                        }
                                        if let Some(rect) = effective_opt {
                                            index.insert(idx, rect);
                                        }
                                    }
                                }
                                for inner_cmd in &commands[idx..=end_idx] {
                                    match inner_cmd {
                                        RenderCommand::PushClip { .. }
                                        | RenderCommand::PushClipStroke { .. } => {
                                            let cur = clip_stack
                                                .last()
                                                .copied()
                                                .unwrap_or(Some(page_rect));
                                            let next = cur
                                                .and_then(|r| tighten_rect_to_clip(r, inner_cmd));
                                            clip_stack.push(next);
                                        }
                                        RenderCommand::PopClip => {
                                            clip_stack.pop();
                                        }
                                        _ => {}
                                    }
                                }
                                idx = end_idx + 1;
                                continue;
                            } else if group_alpha > 0.0 {
                                if let Some(clip_rect) = active_clip {
                                    index.insert(idx, clip_rect);
                                }
                            }
                        } else if *isolated || *knockout || group_alpha < 1.0 || mask.is_some() {
                            isolated_normal_depth = 1;
                        }
                    } else {
                        isolated_normal_depth += 1;
                    }
                }
                RenderCommand::PopBlendGroup => {
                    isolated_normal_depth = isolated_normal_depth.saturating_sub(1);
                }
                other
                    if isolated_normal_depth == 0
                        && (command_has_renderable_overprint(other, font_cache)
                            || command_uses_tiling_backdrop(other, font_cache)) =>
                {
                    if let Some(clip_rect) = clip_stack.last().copied().unwrap_or(Some(page_rect)) {
                        let draw_rect = match compute_commands_bounds(
                            std::slice::from_ref(other),
                            (0.0, 0.0),
                            font_cache,
                        ) {
                            CommandBounds::Empty => None,
                            CommandBounds::Known(r) => {
                                Some(Rect::new(r.x0 - 1.0, r.y0 - 1.0, r.x1 + 1.0, r.y1 + 1.0))
                            }
                            CommandBounds::Unknown => Some(clip_rect),
                        };
                        if let Some(rect) = draw_rect.and_then(|r| intersect_rect(r, clip_rect)) {
                            index.insert(idx, rect);
                        }
                    }
                }
                _ => {}
            }
            idx += 1;
        }
        index
    }

    fn insert(&mut self, start_idx: usize, rect: Rect) {
        if let Some((tx0, tx1, ty0, ty1)) =
            BackdropIndex::cell_range(self.origin_x, self.origin_y, rect)
        {
            let cols = tx1 as i64 - tx0 as i64 + 1;
            let rows = ty1 as i64 - ty0 as i64 + 1;
            if cols > 0 && rows > 0 && cols.saturating_mul(rows) <= 16 {
                for ty in ty0..=ty1 {
                    for tx in tx0..=tx1 {
                        self.buckets
                            .entry((tx, ty))
                            .or_default()
                            .push((start_idx, rect));
                    }
                }
                return;
            }
            self.large_entries.push((start_idx, rect));
            return;
        }
        self.max_unbounded_start = Some(
            self.max_unbounded_start
                .map_or(start_idx, |prev| prev.max(start_idx)),
        );
    }

    fn overlaps_at_or_after(&self, min_start: usize, bounds: Option<Rect>) -> bool {
        if self.max_unbounded_start.is_none()
            && self.large_entries.is_empty()
            && self.buckets.is_empty()
        {
            return false;
        }
        if self.max_unbounded_start.is_some_and(|s| s >= min_start) {
            return true;
        }
        let Some(rect) = bounds else {
            return self.large_entries.iter().any(|&(s, _)| s >= min_start)
                || self
                    .buckets
                    .values()
                    .any(|entries| entries.iter().any(|&(s, _)| s >= min_start));
        };
        if self
            .large_entries
            .iter()
            .any(|&(s, b)| s >= min_start && intersect_rect(b, rect).is_some())
        {
            return true;
        }
        let Some((tx0, tx1, ty0, ty1)) =
            BackdropIndex::cell_range(self.origin_x, self.origin_y, rect)
        else {
            return true;
        };
        let cols = tx1 as i64 - tx0 as i64 + 1;
        let rows = ty1 as i64 - ty0 as i64 + 1;
        if cols <= 0 || rows <= 0 || cols.saturating_mul(rows) > 16 {
            return self.buckets.values().any(|entries| {
                entries
                    .iter()
                    .any(|&(s, b)| s >= min_start && intersect_rect(b, rect).is_some())
            });
        }
        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                if let Some(entries) = self.buckets.get(&(tx, ty)) {
                    if entries
                        .iter()
                        .any(|&(s, b)| s >= min_start && intersect_rect(b, rect).is_some())
                    {
                        return true;
                    }
                }
            }
        }
        false
    }
}

fn command_has_visible_overprint(cmd: &RenderCommand) -> bool {
    match cmd {
        RenderCommand::FillPath {
            paint,
            alpha,
            overprint,
            ..
        }
        | RenderCommand::StrokePath {
            paint,
            alpha,
            overprint,
            ..
        } => effective_paint_alpha(paint, *alpha) > 0.0 && overprint.is_some(),
        RenderCommand::DrawGlyphRun(run) => {
            !run.glyphs.is_empty()
                && effective_paint_alpha(&run.paint, run.alpha) > 0.0
                && run.overprint.is_some()
        }
        _ => false,
    }
}

/// Whether `cmd` carries a visible partial-colorant overprint that `CpuRenderer`
/// can actually rasterize. Fully transparent paint (`effective_paint_alpha <= 0`)
/// and outline-less metrics-only fonts (`!has_font_data()`) do not contribute
/// raster overprint pixels and must not force group flattening or artwork fallback.
fn command_has_renderable_overprint(cmd: &RenderCommand, font_cache: &FontCache) -> bool {
    match cmd {
        RenderCommand::FillPath {
            paint,
            alpha,
            overprint,
            ..
        }
        | RenderCommand::StrokePath {
            paint,
            alpha,
            overprint,
            ..
        } => {
            effective_paint_alpha(paint, *alpha) > 0.0 && overprint.is_some_and(|op| op.active != 0)
        }
        RenderCommand::DrawGlyphRun(run) => {
            !run.glyphs.is_empty()
                && effective_paint_alpha(&run.paint, run.alpha) > 0.0
                && run.overprint.is_some_and(|op| op.active != 0)
                && font_cache
                    .get(run.font_id)
                    .is_some_and(|f| f.has_font_data())
        }
        _ => false,
    }
}

#[derive(Clone)]
struct PlannedScope {
    body_start: usize,
    pop_idx: usize,
    bounds: Rect,
    extra_clips: Vec<usize>,
}

struct PlannedNativeText {
    rel_idx: usize,
    scope_chain: Vec<PlannedScope>,
}

/// Supported soft-mask nesting depth analyzed before falling back to an
/// uncached conservative `Unbounded` result (matches `CpuRenderer`'s default
/// `DocumentLimits::max_blend_group_depth = 16`).
const MAX_SOFT_MASK_ANALYSIS_DEPTH: u8 = 16;

#[derive(Debug, Clone)]
enum MissingTextCoverage {
    /// No visible outline-less text anywhere in the mask (or nested masks).
    None,
    /// Separate (disjoint) bounding regions of visible outline-less text in the
    /// mask so opposite-edge glyphs do not merge across an empty center gap.
    Regions(Vec<Rect>),
    /// At least one visible outline-less glyph (or depth-limited nested mask) has unknown extent.
    Unbounded,
}

impl MissingTextCoverage {
    fn is_empty(&self) -> bool {
        matches!(self, Self::None)
    }

    fn overlaps_rect(&self, target: Rect) -> bool {
        match self {
            Self::None => false,
            Self::Unbounded => true,
            Self::Regions(rects) => rects.iter().any(|&r| intersect_rect(r, target).is_some()),
        }
    }

    fn union_rect(&mut self, r: Rect) {
        let mut r = r.normalize();
        if !(r.x0.is_finite() && r.y0.is_finite() && r.x1.is_finite() && r.y1.is_finite()) {
            *self = Self::Unbounded;
            return;
        }
        match self {
            Self::Unbounded => {}
            Self::None => *self = Self::Regions(vec![r]),
            Self::Regions(rects) => {
                // Only merge two rectangles when their union is STILL an exact
                // rectangle (containment or exact shared edge span) so L-shaped
                // or disjoint overlaps never fill in uncovered corners or gaps.
                let mut merged = true;
                while merged {
                    merged = false;
                    let mut i = 0;
                    while i < rects.len() {
                        let o = rects[i];
                        if rect_contains(o, r) {
                            return;
                        }
                        if rect_contains(r, o) {
                            rects.swap_remove(i);
                            continue;
                        }
                        let same_x = (o.x0 - r.x0).abs() <= 1e-6 && (o.x1 - r.x1).abs() <= 1e-6;
                        let touch_y = o.y0 <= r.y1 + 1e-6 && r.y0 <= o.y1 + 1e-6;
                        let same_y = (o.y0 - r.y0).abs() <= 1e-6 && (o.y1 - r.y1).abs() <= 1e-6;
                        let touch_x = o.x0 <= r.x1 + 1e-6 && r.x0 <= o.x1 + 1e-6;
                        if (same_x && touch_y) || (same_y && touch_x) {
                            rects.swap_remove(i);
                            r = Rect::new(
                                r.x0.min(o.x0),
                                r.y0.min(o.y0),
                                r.x1.max(o.x1),
                                r.y1.max(o.y1),
                            );
                            merged = true;
                        } else {
                            i += 1;
                        }
                    }
                }
                const MAX_MISSING_REGIONS: usize = 256;
                if rects.len() >= MAX_MISSING_REGIONS {
                    let mut best_idx = 0usize;
                    let mut best_added = f64::INFINITY;
                    for (idx, o) in rects.iter().enumerate() {
                        let u = Rect::new(
                            r.x0.min(o.x0),
                            r.y0.min(o.y0),
                            r.x1.max(o.x1),
                            r.y1.max(o.y1),
                        );
                        let u_area = u.width() * u.height();
                        let added = u_area - o.width() * o.height();
                        if added < best_added {
                            best_added = added;
                            best_idx = idx;
                        }
                    }
                    let o = rects[best_idx];
                    rects[best_idx] = Rect::new(
                        r.x0.min(o.x0),
                        r.y0.min(o.y0),
                        r.x1.max(o.x1),
                        r.y1.max(o.y1),
                    );
                } else {
                    rects.push(r);
                }
            }
        }
    }

    fn shift(&self, offset: (f32, f32)) -> Self {
        match self {
            Self::None => Self::None,
            Self::Regions(rects) => {
                let dx = offset.0 as f64;
                let dy = offset.1 as f64;
                if !(dx.is_finite() && dy.is_finite()) {
                    return Self::Unbounded;
                }
                let shifted = rects
                    .iter()
                    .map(|&r| Rect::new(r.x0 + dx, r.y0 + dy, r.x1 + dx, r.y1 + dy))
                    .collect();
                Self::Regions(shifted)
            }
            Self::Unbounded => Self::Unbounded,
        }
    }

    fn enclosing_rect(&self) -> Option<Rect> {
        let Self::Regions(rects) = self else {
            return None;
        };
        let mut iter = rects.iter().copied();
        let first = iter.next()?;
        Some(iter.fold(first, |acc, r| {
            Rect::new(
                acc.x0.min(r.x0),
                acc.y0.min(r.y0),
                acc.x1.max(r.x1),
                acc.y1.max(r.y1),
            )
        }))
    }
}

#[derive(Debug, Clone, Copy)]
enum RenderableMaskBounds {
    /// Proven zero renderable ink (empty `/S /Alpha` mask with no transfer function).
    Empty,
    /// Renderable coverage is confined within `Rect`.
    Bounded(Rect),
    /// Renderable coverage may extend anywhere (luminosity mask, transfer function, or unknown bounds).
    Unbounded,
}

#[derive(Debug, Clone)]
struct SoftMaskAnalysis {
    renderable: RenderableMaskBounds,
    missing_text: MissingTextCoverage,
}

impl SoftMaskAnalysis {
    fn is_proven_empty(&self) -> bool {
        matches!(self.renderable, RenderableMaskBounds::Empty)
            && matches!(self.missing_text, MissingTextCoverage::None)
    }

    fn potential_bounds(&self) -> RenderableMaskBounds {
        match (self.renderable, &self.missing_text) {
            (RenderableMaskBounds::Unbounded, _) | (_, MissingTextCoverage::Unbounded) => {
                RenderableMaskBounds::Unbounded
            }
            (RenderableMaskBounds::Empty, MissingTextCoverage::None) => RenderableMaskBounds::Empty,
            (RenderableMaskBounds::Bounded(r), MissingTextCoverage::None) => {
                RenderableMaskBounds::Bounded(r)
            }
            (RenderableMaskBounds::Empty, MissingTextCoverage::Regions(_)) => self
                .missing_text
                .enclosing_rect()
                .map_or(RenderableMaskBounds::Empty, RenderableMaskBounds::Bounded),
            (RenderableMaskBounds::Bounded(r), MissingTextCoverage::Regions(_)) => {
                match self.missing_text.enclosing_rect() {
                    Some(m) => RenderableMaskBounds::Bounded(Rect::new(
                        r.x0.min(m.x0),
                        r.y0.min(m.y0),
                        r.x1.max(m.x1),
                        r.y1.max(m.y1),
                    )),
                    None => RenderableMaskBounds::Bounded(r),
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
struct MaskContentSummary {
    missing_text: MissingTextCoverage,
    renderable_bounds: Option<CommandBounds>,
}

/// Per-slide cache of soft-mask display-list analysis keyed by the shared
/// `Arc<DisplayList>` address so repeating one M-command mask across N objects
/// walks its commands once instead of N × M times.
///
/// Any result affected by `MAX_SOFT_MASK_ANALYSIS_DEPTH` is kept out of the
/// cache so a deep evaluation never poisons a shallower reuse of the same mask.
#[derive(Default)]
struct SoftMaskAnalysisCache {
    entries: std::collections::HashMap<usize, MaskContentSummary>,
}

impl SoftMaskAnalysisCache {
    fn analyze(
        &mut self,
        mask: Option<&zpdf_display_list::SoftMask>,
        font_cache: &FontCache,
        stats: &mut ConversionStats,
    ) -> SoftMaskAnalysis {
        let Some(mask) = mask else {
            return SoftMaskAnalysis {
                renderable: RenderableMaskBounds::Unbounded,
                missing_text: MissingTextCoverage::None,
            };
        };
        self.analyze_inner(mask, font_cache, stats, 0).0
    }

    fn analyze_inner(
        &mut self,
        mask: &zpdf_display_list::SoftMask,
        font_cache: &FontCache,
        stats: &mut ConversionStats,
        depth: u8,
    ) -> (SoftMaskAnalysis, bool) {
        if depth >= MAX_SOFT_MASK_ANALYSIS_DEPTH {
            return (
                SoftMaskAnalysis {
                    renderable: RenderableMaskBounds::Unbounded,
                    missing_text: MissingTextCoverage::Unbounded,
                },
                true,
            );
        }
        let need_renderable =
            mask.kind == zpdf_display_list::SoftMaskKind::Alpha && mask.transfer.is_none();
        let key = std::sync::Arc::as_ptr(&mask.commands) as usize;
        let (summary, depth_limited) = if let Some(existing) = self.entries.get(&key).cloned() {
            if need_renderable && existing.renderable_bounds.is_none() {
                let bounds =
                    compute_commands_bounds(&mask.commands.commands, (0.0, 0.0), font_cache);
                let updated = MaskContentSummary {
                    missing_text: existing.missing_text,
                    renderable_bounds: Some(bounds),
                };
                self.entries.insert(key, updated.clone());
                (updated, false)
            } else {
                (existing, false)
            }
        } else {
            let (missing_text, depth_limited) =
                self.scan_missing_text(&mask.commands.commands, font_cache, stats, depth);
            let renderable_bounds = need_renderable
                .then(|| compute_commands_bounds(&mask.commands.commands, (0.0, 0.0), font_cache));
            let created = MaskContentSummary {
                missing_text,
                renderable_bounds,
            };
            if !depth_limited {
                self.entries.insert(key, created.clone());
            }
            (created, depth_limited)
        };

        let renderable = if !need_renderable {
            RenderableMaskBounds::Unbounded
        } else {
            match summary.renderable_bounds.unwrap_or(CommandBounds::Unknown) {
                CommandBounds::Empty => RenderableMaskBounds::Empty,
                CommandBounds::Known(r) => {
                    match shift_rect(r, mask.offset.0 as f64, mask.offset.1 as f64) {
                        Some(sr) => RenderableMaskBounds::Bounded(Rect::new(
                            sr.x0 - 1.0,
                            sr.y0 - 1.0,
                            sr.x1 + 1.0,
                            sr.y1 + 1.0,
                        )),
                        None => RenderableMaskBounds::Unbounded,
                    }
                }
                CommandBounds::Unknown => RenderableMaskBounds::Unbounded,
            }
        };

        (
            SoftMaskAnalysis {
                renderable,
                missing_text: summary.missing_text.shift(mask.offset),
            },
            depth_limited,
        )
    }

    fn scan_missing_text(
        &mut self,
        commands: &[RenderCommand],
        font_cache: &FontCache,
        stats: &mut ConversionStats,
        depth: u8,
    ) -> (MissingTextCoverage, bool) {
        let mut missing = MissingTextCoverage::None;
        let mut depth_limited = false;
        let mut clips: Vec<&RenderCommand> = Vec::new();
        // Active nested alpha-mask potential coverage bounds: `(pop_idx, cumulative_rect)`.
        let mut mask_bounds_stack: Vec<(usize, Rect)> = Vec::new();
        let mut idx = 0usize;
        while idx < commands.len() {
            stats.soft_mask_analysis_visits += 1;
            let cmd = &commands[idx];
            let active_mask_rect = mask_bounds_stack.last().map(|&(_, r)| r);
            match cmd {
                RenderCommand::PushClip { .. } | RenderCommand::PushClipStroke { .. } => {
                    clips.push(cmd);
                    idx += 1;
                }
                RenderCommand::PopClip => {
                    clips.pop();
                    idx += 1;
                }
                RenderCommand::PushBlendGroup { alpha, mask, .. } => {
                    if !alpha.is_finite() || *alpha <= 0.0 {
                        if let Some(sub_end) =
                            SlideConverter::find_matching_pop_blend(commands, idx)
                        {
                            idx = sub_end + 1;
                            continue;
                        }
                        break;
                    }
                    if let Some(m) = mask {
                        let (sub_a, sub_limited) =
                            self.analyze_inner(m, font_cache, stats, depth + 1);
                        if sub_limited {
                            depth_limited = true;
                        }
                        if sub_a.is_proven_empty() {
                            if let Some(sub_end) =
                                SlideConverter::find_matching_pop_blend(commands, idx)
                            {
                                idx = sub_end + 1;
                                continue;
                            }
                            break;
                        }
                        let sub_end = SlideConverter::find_matching_pop_blend(commands, idx)
                            .unwrap_or(commands.len() - 1);
                        let sub_slice = &commands[idx..=sub_end];
                        let potential = sub_a.potential_bounds();
                        match sub_a.missing_text {
                            MissingTextCoverage::None => {}
                            MissingTextCoverage::Regions(sub_rects) => {
                                for mr in sub_rects {
                                    let constrained =
                                        tighten_rect_to_clips(mr, &clips).and_then(|c| {
                                            match active_mask_rect {
                                                Some(am) => intersect_rect(c, am),
                                                None => Some(c),
                                            }
                                        });
                                    if let Some(clipped_mr) = constrained {
                                        if group_has_visible_paint(
                                            sub_slice, &clips, clipped_mr, font_cache, self, stats,
                                        ) {
                                            missing.union_rect(clipped_mr);
                                        }
                                    }
                                }
                            }
                            MissingTextCoverage::Unbounded => {
                                let bound = match active_clip_rect_for(&clips) {
                                    ActiveClip::Rect(None) => Some(None),
                                    ActiveClip::Rect(Some(cr)) => Some(match active_mask_rect {
                                        Some(am) => intersect_rect(cr, am),
                                        None => Some(cr),
                                    }),
                                    ActiveClip::None | ActiveClip::Complex => {
                                        active_mask_rect.map(Some)
                                    }
                                };
                                match bound {
                                    Some(None) => {}
                                    Some(Some(br)) => {
                                        if group_has_visible_paint(
                                            sub_slice, &clips, br, font_cache, self, stats,
                                        ) {
                                            missing.union_rect(br);
                                        }
                                    }
                                    None => {
                                        let wide = Rect::new(-1e12, -1e12, 1e12, 1e12);
                                        if group_has_visible_paint(
                                            sub_slice, &clips, wide, font_cache, self, stats,
                                        ) {
                                            missing = MissingTextCoverage::Unbounded;
                                        }
                                    }
                                }
                            }
                        }
                        match potential {
                            RenderableMaskBounds::Empty => {
                                idx = sub_end + 1;
                                continue;
                            }
                            RenderableMaskBounds::Bounded(mr) => {
                                let effective = match active_mask_rect {
                                    Some(am) => intersect_rect(mr, am),
                                    None => Some(mr),
                                };
                                let Some(eff_rect) = effective else {
                                    idx = sub_end + 1;
                                    continue;
                                };
                                if matches!(sub_a.renderable, RenderableMaskBounds::Empty) {
                                    idx = sub_end + 1;
                                    continue;
                                }
                                mask_bounds_stack.push((sub_end, eff_rect));
                            }
                            RenderableMaskBounds::Unbounded => {
                                if matches!(sub_a.renderable, RenderableMaskBounds::Empty) {
                                    idx = sub_end + 1;
                                    continue;
                                }
                            }
                        }
                    }
                    idx += 1;
                }
                RenderCommand::PopBlendGroup => {
                    while mask_bounds_stack
                        .last()
                        .is_some_and(|&(pop_idx, _)| pop_idx == idx)
                    {
                        mask_bounds_stack.pop();
                    }
                    idx += 1;
                }
                RenderCommand::DrawGlyphRun(run)
                    if is_visible_native_only_glyph_run(cmd, font_cache) =>
                {
                    match native_text_ink_bounds(run, font_cache) {
                        Some(r) => {
                            let constrained =
                                tighten_rect_to_clips(r, &clips).and_then(|clipped| {
                                    let padded = Rect::new(
                                        clipped.x0 - 1.0,
                                        clipped.y0 - 1.0,
                                        clipped.x1 + 1.0,
                                        clipped.y1 + 1.0,
                                    );
                                    match active_mask_rect {
                                        Some(am) => intersect_rect(padded, am),
                                        None => Some(padded),
                                    }
                                });
                            if let Some(region) = constrained {
                                missing.union_rect(region);
                            }
                        }
                        None => match active_clip_rect_for(&clips) {
                            ActiveClip::Rect(None) => {}
                            ActiveClip::Rect(Some(cr)) => {
                                let constrained = match active_mask_rect {
                                    Some(am) => intersect_rect(cr, am),
                                    None => Some(cr),
                                };
                                if let Some(region) = constrained {
                                    missing.union_rect(region);
                                }
                            }
                            ActiveClip::None | ActiveClip::Complex => {
                                if let Some(am) = active_mask_rect {
                                    missing.union_rect(am);
                                } else {
                                    missing = MissingTextCoverage::Unbounded;
                                }
                            }
                        },
                    }
                    idx += 1;
                }
                _ => {
                    idx += 1;
                }
            }
        }
        (missing, depth_limited)
    }
}

fn shift_rect(r: Rect, dx: f64, dy: f64) -> Option<Rect> {
    if !(dx.is_finite() && dy.is_finite()) {
        return None;
    }
    Some(Rect::new(r.x0 + dx, r.y0 + dy, r.x1 + dx, r.y1 + dy))
}

fn group_overlaps_missing_mask_text(
    missing_text: &MissingTextCoverage,
    group_cmds: &[RenderCommand],
    outer_clips: &[&RenderCommand],
    visible_region: Rect,
    font_cache: &FontCache,
    soft_mask_cache: &mut SoftMaskAnalysisCache,
    stats: &mut ConversionStats,
) -> bool {
    match missing_text {
        MissingTextCoverage::None => false,
        MissingTextCoverage::Unbounded => group_has_visible_paint(
            group_cmds,
            outer_clips,
            visible_region,
            font_cache,
            soft_mask_cache,
            stats,
        ),
        MissingTextCoverage::Regions(rects) => rects.iter().any(|&mr| {
            intersect_rect(mr, visible_region).is_some_and(|check_region| {
                group_has_visible_paint(
                    group_cmds,
                    outer_clips,
                    check_region,
                    font_cache,
                    soft_mask_cache,
                    stats,
                )
            })
        }),
    }
}

fn group_has_visible_paint(
    group_cmds: &[RenderCommand],
    outer_clips: &[&RenderCommand],
    visible_region: Rect,
    font_cache: &FontCache,
    soft_mask_cache: &mut SoftMaskAnalysisCache,
    stats: &mut ConversionStats,
) -> bool {
    let mut clips: Vec<&RenderCommand> = outer_clips.to_vec();
    let mut region_stack: Vec<(usize, Rect)> = Vec::new();
    let mut idx = 0usize;
    while idx < group_cmds.len() {
        let cmd = &group_cmds[idx];
        let active_region = region_stack
            .last()
            .map(|&(_, r)| r)
            .unwrap_or(visible_region);
        match cmd {
            RenderCommand::PushClip { .. } | RenderCommand::PushClipStroke { .. } => {
                clips.push(cmd);
                idx += 1;
            }
            RenderCommand::PopClip => {
                clips.pop();
                idx += 1;
            }
            RenderCommand::PushBlendGroup { alpha, mask, .. } if idx > 0 => {
                if !alpha.is_finite() || *alpha <= 0.0 {
                    if let Some(sub_end) = SlideConverter::find_matching_pop_blend(group_cmds, idx)
                    {
                        idx = sub_end + 1;
                        continue;
                    }
                    break;
                }
                if let Some(m) = mask {
                    let sub_a = soft_mask_cache.analyze(Some(m), font_cache, stats);
                    let sub_end = SlideConverter::find_matching_pop_blend(group_cmds, idx);
                    match sub_a.potential_bounds() {
                        RenderableMaskBounds::Empty => {
                            if let Some(end) = sub_end {
                                idx = end + 1;
                                continue;
                            }
                            break;
                        }
                        RenderableMaskBounds::Bounded(mr) => {
                            let Some(inter) = intersect_rect(mr, active_region) else {
                                if let Some(end) = sub_end {
                                    idx = end + 1;
                                    continue;
                                }
                                break;
                            };
                            if let Some(end) = sub_end {
                                region_stack.push((end, inter));
                            }
                        }
                        RenderableMaskBounds::Unbounded => {}
                    }
                }
                idx += 1;
            }
            RenderCommand::PopBlendGroup => {
                while region_stack
                    .last()
                    .is_some_and(|&(pop_idx, _)| pop_idx == idx)
                {
                    region_stack.pop();
                }
                idx += 1;
            }
            RenderCommand::DrawGlyphRun(run)
                if is_visible_native_only_glyph_run(cmd, font_cache) =>
            {
                if is_rect_visible_under_clips(
                    native_text_ink_bounds(run, font_cache),
                    &clips,
                    active_region,
                ) {
                    return true;
                }
                idx += 1;
            }
            other if is_visible_paint_command(other, font_cache) => {
                match compute_commands_bounds(std::slice::from_ref(other), (0.0, 0.0), font_cache) {
                    CommandBounds::Empty => {}
                    CommandBounds::Known(r) => {
                        if is_rect_visible_under_clips(Some(r), &clips, active_region) {
                            return true;
                        }
                    }
                    CommandBounds::Unknown => {
                        let visible = match active_clip_rect_for(&clips) {
                            ActiveClip::None | ActiveClip::Complex => true,
                            ActiveClip::Rect(None) => false,
                            ActiveClip::Rect(Some(cr)) => {
                                intersect_rect(cr, active_region).is_some()
                            }
                        };
                        if visible {
                            return true;
                        }
                    }
                }
                idx += 1;
            }
            _ => {
                idx += 1;
            }
        }
    }
    false
}

fn command_has_backdrop_dependent_native_overprint(
    cmd: &RenderCommand,
    font_cache: &FontCache,
) -> bool {
    let RenderCommand::DrawGlyphRun(run) = cmd else {
        return false;
    };
    let effective_alpha = effective_paint_alpha(&run.paint, run.alpha);
    if run.glyphs.is_empty() || effective_alpha <= 0.0 {
        return false;
    }
    let Some(op) = run.overprint.filter(|op| op.active != 0) else {
        return false;
    };
    if font_cache
        .get(run.font_id)
        .is_some_and(|f| f.has_font_data())
    {
        return false;
    }
    !(effective_alpha >= 1.0 - 1e-3
        && ((op.paints(3) && op.cmyk[3] >= 1.0 - 1e-3) || op.active == 0b1111))
}

fn track_blend_non_passthrough(cmd: &RenderCommand, stack: &mut Vec<bool>) {
    match cmd {
        RenderCommand::PushBlendGroup {
            blend_mode,
            isolated,
            knockout,
            alpha,
            mask,
            ..
        } => {
            let group_alpha = if alpha.is_finite() {
                alpha.clamp(0.0, 1.0)
            } else {
                0.0
            };
            let passthrough = !*isolated
                && !*knockout
                && *blend_mode == zpdf_display_list::BlendMode::Normal
                && group_alpha >= 1.0
                && mask.is_none();
            stack.push(!passthrough);
        }
        RenderCommand::PopBlendGroup => {
            stack.pop();
        }
        _ => {}
    }
}

/// Resolve an overprinting color + opacity over one premultiplied RGBA8 backdrop
/// pixel, matching `CpuRenderer::overprint_merge` (`out_a = ba + a * (1 - ba)`):
/// - `px[3] == 0`: transparent isolated-Form backdrop (white paper inks, `out_a = alpha`)
/// - `px[3] == 255`: opaque backdrop (resolved color over `px[0..3]`, `out_a = 1.0`)
/// - `0 < px[3] < 255`: unpremultiply the translucent Form backdrop and solve
///   for the exact SourceOver `TextBox` color `C_tb` with opacity `alpha` that
///   matches `out_a * C_out + (1 - out_a) * P` over every outer page color `P`.
///   Opaque overprints (`alpha == 1`) always yield `(C_out, 1.0)`; translucent
///   overprints succeed iff `C_tb` lies in `[0, 1]^3`, otherwise return `Err`.
fn resolve_overprint_on_premul_rgba(
    px: [u8; 4],
    op: &zpdf_display_list::Overprint,
    alpha: f32,
) -> Result<((u8, u8, u8), f32)> {
    let eff_a = if alpha.is_finite() {
        alpha.clamp(0.0, 1.0)
    } else {
        0.0
    };
    match px[3] {
        0 => Ok((
            resolve_overprint_color_on_rgb((255, 255, 255), op, eff_a),
            eff_a,
        )),
        255 => Ok((
            resolve_overprint_color_on_rgb((px[0], px[1], px[2]), op, eff_a),
            1.0,
        )),
        a_u8 => {
            let a = eff_a as f64;
            if a <= 0.0 {
                return Ok(((0, 0, 0), 0.0));
            }
            let ba = a_u8 as f64 / 255.0;
            let bg = [
                (px[0] as f64 / a_u8 as f64).clamp(0.0, 1.0),
                (px[1] as f64 / a_u8 as f64).clamp(0.0, 1.0),
                (px[2] as f64 / a_u8 as f64).clamp(0.0, 1.0),
            ];
            let (bc, bm, by, bk) = zpdf_core::rgb_to_cmyk_naive(bg[0], bg[1], bg[2]);
            let bd = [bc, bm, by, bk];
            let mut res = [0.0f64; 4];
            for i in 0..4 {
                let tgt = if op.paints(i) {
                    if op.cmyk[i].is_finite() {
                        op.cmyk[i].clamp(0.0, 1.0) as f64
                    } else {
                        0.0
                    }
                } else {
                    bd[i]
                };
                res[i] = bd[i] + a * (tgt - bd[i]);
            }
            let (rr, rg, rb) = zpdf_core::cmyk_to_rgb_naive(res[0], res[1], res[2], res[3]);
            let enc = |v: f64| (v * 255.0).round().clamp(0.0, 255.0) as u8;
            if eff_a >= 1.0 - 1e-4 {
                // Opaque overprint replaces the translucent Form pixel with an
                // opaque pixel (`out_a = 1.0`), directly representable as an
                // opaque `TextBox` on top of the translucent Form shape.
                return Ok(((enc(rr), enc(rg), enc(rb)), 1.0));
            }
            let out_a = ba + a * (1.0 - ba);
            let out_rgb = [rr, rg, rb];
            let mut tb_rgb = [0u8; 3];
            for k in 0..3 {
                let c_tb = (out_a * out_rgb[k] - (1.0 - a) * ba * bg[k]) / a;
                if !(-1e-3..=1.0 + 1e-3).contains(&c_tb) {
                    return Err(zpdf_core::Error::StreamDecode(
                        "PPTX overprinting native-only text without font outlines spans a translucent backdrop that cannot be represented faithfully".into(),
                    ));
                }
                tb_rgb[k] = enc(c_tb);
            }
            Ok(((tb_rgb[0], tb_rgb[1], tb_rgb[2]), eff_a))
        }
    }
}

fn source_over_premul(fg_rgb: (u8, u8, u8), fg_alpha: f32, bg: [u8; 4]) -> [u8; 4] {
    let sa = if fg_alpha.is_finite() {
        fg_alpha.clamp(0.0, 1.0) as f64
    } else {
        0.0
    };
    if sa <= 0.0 {
        return bg;
    }
    if sa >= 1.0 - 1e-4 {
        return [fg_rgb.0, fg_rgb.1, fg_rgb.2, 255];
    }
    let inv = 1.0 - sa;
    let sr = fg_rgb.0 as f64 * sa;
    let sg = fg_rgb.1 as f64 * sa;
    let sb = fg_rgb.2 as f64 * sa;
    let ba = bg[3] as f64 / 255.0;
    let out_a = sa + ba * inv;
    let enc = |v: f64| v.round().clamp(0.0, 255.0) as u8;
    [
        enc(sr + bg[0] as f64 * inv),
        enc(sg + bg[1] as f64 * inv),
        enc(sb + bg[2] as f64 * inv),
        enc(out_a * 255.0),
    ]
}

#[derive(Debug, Clone, Copy)]
struct ResolvedOverprintSample {
    rgb: (u8, u8, u8),
    alpha: f32,
    uniform_bg_px: Option<[u8; 4]>,
}

fn resolve_uniform_overprint_in_surface(
    surf: (u32, u32, &[u8]),
    surf_rect: Rect,
    scale: f32,
    target_rect: Rect,
    op: &zpdf_display_list::Overprint,
    effective_alpha: f32,
    empty_bg_rgba: [u8; 4],
) -> Result<ResolvedOverprintSample> {
    let (surf_w, surf_h, data) = surf;
    let s = scale as f64;
    if !(s.is_finite() && s > 0.0) || surf_w == 0 || surf_h == 0 {
        let (rgb, alpha) = resolve_overprint_on_premul_rgba(empty_bg_rgba, op, effective_alpha)?;
        return Ok(ResolvedOverprintSample {
            rgb,
            alpha,
            uniform_bg_px: Some(empty_bg_rgba),
        });
    }
    let px0 = (((target_rect.x0 - surf_rect.x0) * s).floor().max(0.0) as u32).min(surf_w);
    let py0 = (((surf_rect.y1 - target_rect.y1) * s).floor().max(0.0) as u32).min(surf_h);
    let px1 = (((target_rect.x1 - surf_rect.x0) * s).ceil().max(0.0) as u32).min(surf_w);
    let py1 = (((surf_rect.y1 - target_rect.y0) * s).ceil().max(0.0) as u32).min(surf_h);
    if px1 <= px0 || py1 <= py0 {
        let (rgb, alpha) = resolve_overprint_on_premul_rgba(empty_bg_rgba, op, effective_alpha)?;
        return Ok(ResolvedOverprintSample {
            rgb,
            alpha,
            uniform_bg_px: Some(empty_bg_rgba),
        });
    }
    let stride = surf_w as usize;
    let mut first_raw: Option<[u8; 4]> = None;
    let mut raw_uniform = true;
    let mut last_raw: Option<[u8; 4]> = None;
    let mut resolved_first: Option<((u8, u8, u8), u8, f32)> = None;
    for y in py0 as usize..py1 as usize {
        let row = y * stride;
        for x in px0 as usize..px1 as usize {
            let off = (row + x) * 4;
            let raw = [data[off], data[off + 1], data[off + 2], data[off + 3]];
            if let Some(fr) = first_raw {
                if raw != fr {
                    raw_uniform = false;
                }
            } else {
                first_raw = Some(raw);
            }
            if last_raw == Some(raw) {
                continue;
            }
            last_raw = Some(raw);
            let (rgb, out_a) = resolve_overprint_on_premul_rgba(raw, op, effective_alpha)?;
            let out_a_u8 = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
            if let Some((first_rgb, first_a_u8, _)) = resolved_first {
                if rgb != first_rgb || out_a_u8 != first_a_u8 {
                    return Err(zpdf_core::Error::StreamDecode(
                        "PPTX overprinting native-only text without font outlines spans a non-uniform backdrop and cannot be rendered faithfully".into(),
                    ));
                }
            } else {
                resolved_first = Some((rgb, out_a_u8, out_a));
            }
        }
    }
    let uniform_bg_px = raw_uniform.then_some(first_raw.unwrap_or(empty_bg_rgba));
    if let Some((rgb, _, alpha)) = resolved_first {
        Ok(ResolvedOverprintSample {
            rgb,
            alpha,
            uniform_bg_px,
        })
    } else {
        let (rgb, alpha) = resolve_overprint_on_premul_rgba(empty_bg_rgba, op, effective_alpha)?;
        Ok(ResolvedOverprintSample {
            rgb,
            alpha,
            uniform_bg_px: Some(empty_bg_rgba),
        })
    }
}

fn resolve_overprint_color_on_rgb(
    bg_rgb: (u8, u8, u8),
    op: &zpdf_display_list::Overprint,
    alpha: f32,
) -> (u8, u8, u8) {
    let a = if alpha.is_finite() {
        alpha.clamp(0.0, 1.0) as f64
    } else {
        0.0
    };
    let (bc, bm, by, bk) = zpdf_core::rgb_to_cmyk_naive(
        bg_rgb.0 as f64 / 255.0,
        bg_rgb.1 as f64 / 255.0,
        bg_rgb.2 as f64 / 255.0,
    );
    let bd = [bc, bm, by, bk];
    let mut res = [0.0f64; 4];
    for i in 0..4 {
        let tgt = if op.paints(i) {
            if op.cmyk[i].is_finite() {
                op.cmyk[i].clamp(0.0, 1.0) as f64
            } else {
                0.0
            }
        } else {
            bd[i]
        };
        res[i] = bd[i] + a * (tgt - bd[i]);
    }
    let (rr, rg, rb) = zpdf_core::cmyk_to_rgb_naive(res[0], res[1], res[2], res[3]);
    let enc = |v: f64| (v * 255.0).round().clamp(0.0, 255.0) as u8;
    (enc(rr), enc(rg), enc(rb))
}

/// Whether flattening `cmds` depends on the already-painted page backdrop
/// (a visible non-Normal blend group, renderable partial-colorant overprint,
/// or backdrop-dependent `Paint::Tiling` not shielded by an enclosing isolated
/// Normal group).
fn group_uses_page_backdrop(cmds: &[RenderCommand], font_cache: &FontCache) -> bool {
    group_uses_page_backdrop_depth(cmds, font_cache, 0)
}

fn command_uses_tiling_backdrop(cmd: &RenderCommand, font_cache: &FontCache) -> bool {
    command_uses_tiling_backdrop_depth(cmd, font_cache, 0)
}

fn command_uses_tiling_backdrop_depth(
    cmd: &RenderCommand,
    font_cache: &FontCache,
    depth: u8,
) -> bool {
    if depth >= 16 {
        return false;
    }
    match cmd {
        RenderCommand::FillPath {
            paint: Paint::Tiling(tiling),
            alpha,
            overprint,
            ..
        }
        | RenderCommand::StrokePath {
            paint: Paint::Tiling(tiling),
            alpha,
            overprint,
            ..
        } => {
            alpha.is_finite()
                && *alpha > 0.0
                && !overprint.is_some_and(|op| op.active == 0)
                && group_uses_page_backdrop_depth(&tiling.commands.commands, font_cache, depth + 1)
        }
        RenderCommand::DrawGlyphRun(run) => {
            if let Paint::Tiling(tiling) = &run.paint {
                run.alpha.is_finite()
                    && run.alpha > 0.0
                    && !run.overprint.is_some_and(|op| op.active == 0)
                    && font_cache
                        .get(run.font_id)
                        .is_some_and(|f| f.has_font_data())
                    && group_uses_page_backdrop_depth(
                        &tiling.commands.commands,
                        font_cache,
                        depth + 1,
                    )
            } else {
                false
            }
        }
        _ => false,
    }
}

fn group_uses_page_backdrop_depth(
    cmds: &[RenderCommand],
    font_cache: &FontCache,
    depth: u8,
) -> bool {
    let mut isolated_normal_depth = 0usize;
    for cmd in cmds {
        match cmd {
            RenderCommand::PushBlendGroup {
                blend_mode,
                isolated,
                knockout,
                alpha,
                mask,
                ..
            } => {
                if isolated_normal_depth == 0 {
                    let group_alpha = if alpha.is_finite() {
                        alpha.clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    if group_alpha <= 0.0 {
                        isolated_normal_depth = 1;
                    } else if *blend_mode != zpdf_display_list::BlendMode::Normal {
                        return true;
                    } else if *isolated || *knockout || group_alpha < 1.0 || mask.is_some() {
                        isolated_normal_depth = 1;
                    }
                } else {
                    isolated_normal_depth += 1;
                }
            }
            RenderCommand::PopBlendGroup => {
                isolated_normal_depth = isolated_normal_depth.saturating_sub(1);
            }
            other
                if isolated_normal_depth == 0
                    && (command_has_renderable_overprint(other, font_cache)
                        || command_uses_tiling_backdrop_depth(other, font_cache, depth)) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn dummy_slide_element() -> SlideElement {
    SlideElement::Image {
        x_emu: 0,
        y_emu: 0,
        width_emu: 0,
        height_emu: 0,
        image_data: Vec::new(),
        image_id: String::new(),
    }
}

fn active_clip_rect_for(clip_stack: &[&RenderCommand]) -> ActiveClip {
    if clip_stack.is_empty() {
        return ActiveClip::None;
    }
    let mut combined: Option<Rect> = None;
    for &clip in clip_stack {
        let RenderCommand::PushClip { path, .. } = clip else {
            return ActiveClip::Complex;
        };
        let Some(rect) = exact_axis_aligned_rect(path) else {
            return ActiveClip::Complex;
        };
        combined = match combined {
            None => Some(rect),
            Some(prev) => match intersect_rect(prev, rect) {
                Some(inter) => Some(inter),
                None => return ActiveClip::Rect(None),
            },
        };
    }
    ActiveClip::Rect(combined)
}

fn clip_stack_constrains_rect_for(clip_stack: &[&RenderCommand], bounds: Option<Rect>) -> bool {
    match active_clip_rect_for(clip_stack) {
        ActiveClip::None => false,
        ActiveClip::Rect(None) => true,
        ActiveClip::Rect(Some(clip)) => match bounds {
            Some(b) => {
                !(b.x0 >= clip.x0 - 1e-3
                    && b.y0 >= clip.y0 - 1e-3
                    && b.x1 <= clip.x1 + 1e-3
                    && b.y1 <= clip.y1 + 1e-3)
            }
            None => true,
        },
        ActiveClip::Complex => true,
    }
}

fn tighten_rect_to_clips(mut rect: Rect, clips: &[&RenderCommand]) -> Option<Rect> {
    for &clip in clips {
        rect = tighten_rect_to_clip(rect, clip)?;
    }
    Some(rect)
}

fn is_rect_visible_under_clips(
    bounds: Option<Rect>,
    clips: &[&RenderCommand],
    page_rect: Rect,
) -> bool {
    let Some(rect) = bounds else {
        return !matches!(active_clip_rect_for(clips), ActiveClip::Rect(None));
    };
    let Some(clipped) = tighten_rect_to_clips(rect, clips) else {
        return false;
    };
    intersect_rect(clipped, page_rect).is_some()
}

fn rect_contains(outer: Rect, inner: Rect) -> bool {
    let o = outer.normalize();
    let i = inner.normalize();
    o.x0 <= i.x0 && o.y0 <= i.y0 && o.x1 >= i.x1 && o.y1 >= i.y1
}

/// Static segment tree over `len` elementary intervals `[0..len)` tracking
/// active coverage counts for O(N log N) 2D rectangle-intersection sweeps.
struct IntervalCountTree {
    len: usize,
    cover: Vec<u32>,
    any: Vec<bool>,
}

impl IntervalCountTree {
    fn new(len: usize) -> Self {
        let cap = (len.max(1) * 4).max(4);
        Self {
            len,
            cover: vec![0; cap],
            any: vec![false; cap],
        }
    }

    fn update(&mut self, ql: usize, qr: usize, delta: i32, visits: &mut usize) {
        if ql >= qr || self.len == 0 {
            return;
        }
        self.update_rec(1, 0, self.len, ql, qr, delta, visits);
    }

    #[allow(clippy::too_many_arguments)]
    fn update_rec(
        &mut self,
        node: usize,
        nl: usize,
        nr: usize,
        ql: usize,
        qr: usize,
        delta: i32,
        visits: &mut usize,
    ) {
        *visits += 1;
        if ql <= nl && nr <= qr {
            if delta > 0 {
                self.cover[node] = self.cover[node].saturating_add(delta as u32);
            } else {
                self.cover[node] = self.cover[node].saturating_sub((-delta) as u32);
            }
            self.any[node] = self.cover[node] > 0
                || (nl + 1 < nr && (self.any[node * 2] || self.any[node * 2 + 1]));
            return;
        }
        let mid = nl + (nr - nl) / 2;
        if ql < mid {
            self.update_rec(node * 2, nl, mid, ql, qr, delta, visits);
        }
        if qr > mid {
            self.update_rec(node * 2 + 1, mid, nr, ql, qr, delta, visits);
        }
        self.any[node] = self.cover[node] > 0 || self.any[node * 2] || self.any[node * 2 + 1];
    }

    fn query_any(&self, ql: usize, qr: usize, visits: &mut usize) -> bool {
        if ql >= qr || self.len == 0 {
            return false;
        }
        self.query_rec(1, 0, self.len, ql, qr, visits)
    }

    fn query_rec(
        &self,
        node: usize,
        nl: usize,
        nr: usize,
        ql: usize,
        qr: usize,
        visits: &mut usize,
    ) -> bool {
        *visits += 1;
        if !self.any[node] {
            return false;
        }
        if self.cover[node] > 0 || (ql <= nl && nr <= qr) {
            return true;
        }
        let mid = nl + (nr - nl) / 2;
        (ql < mid && self.query_rec(node * 2, nl, mid, ql, qr, visits))
            || (qr > mid && self.query_rec(node * 2 + 1, mid, nr, ql, qr, visits))
    }
}

/// Returns `true` iff two rectangles in `rects` strictly intersect in both X
/// and Y (`intersect_rect(a, b).is_some()`) and at least one of the pair has
/// `translucent_changes_bg == true`, in O(N log N) time regardless of layout.
fn has_stacked_translucent_rect_overlap(rects: &[(Rect, bool)], visits: &mut usize) -> bool {
    let mut ys: Vec<f64> = Vec::with_capacity(rects.len() * 2);
    for &(r, _) in rects {
        if r.x1 > r.x0 && r.y1 > r.y0 {
            ys.push(r.y0);
            ys.push(r.y1);
        }
    }
    if ys.len() < 4 {
        return false;
    }
    ys.sort_unstable_by(f64::total_cmp);
    ys.dedup();
    if ys.len() < 2 {
        return false;
    }

    struct SweepEvent {
        x: f64,
        is_enter: bool,
        yl: usize,
        yr: usize,
        is_translucent: bool,
    }
    let mut events: Vec<SweepEvent> = Vec::with_capacity(rects.len() * 2);
    for &(r, is_translucent) in rects {
        if !(r.x1 > r.x0 && r.y1 > r.y0) {
            continue;
        }
        let Ok(yl) = ys.binary_search_by(|y| y.total_cmp(&r.y0)) else {
            continue;
        };
        let Ok(yr) = ys.binary_search_by(|y| y.total_cmp(&r.y1)) else {
            continue;
        };
        if yl >= yr {
            continue;
        }
        events.push(SweepEvent {
            x: r.x0,
            is_enter: true,
            yl,
            yr,
            is_translucent,
        });
        events.push(SweepEvent {
            x: r.x1,
            is_enter: false,
            yl,
            yr,
            is_translucent,
        });
    }
    // Exits precede enters at equal x so touching edges (`a.x1 == b.x0`) do not conflict.
    events.sort_unstable_by(|a, b| a.x.total_cmp(&b.x).then(a.is_enter.cmp(&b.is_enter)));

    let num_intervals = ys.len() - 1;
    let mut active_translucent = IntervalCountTree::new(num_intervals);
    let mut active_opaque = IntervalCountTree::new(num_intervals);
    for ev in events {
        if ev.is_enter {
            if ev.is_translucent {
                if active_translucent.query_any(ev.yl, ev.yr, visits)
                    || active_opaque.query_any(ev.yl, ev.yr, visits)
                {
                    return true;
                }
                active_translucent.update(ev.yl, ev.yr, 1, visits);
            } else {
                if active_translucent.query_any(ev.yl, ev.yr, visits) {
                    return true;
                }
                active_opaque.update(ev.yl, ev.yr, 1, visits);
            }
        } else if ev.is_translucent {
            active_translucent.update(ev.yl, ev.yr, -1, visits);
        } else {
            active_opaque.update(ev.yl, ev.yr, -1, visits);
        }
    }
    false
}

fn native_text_ink_bounds(
    run: &zpdf_display_list::GlyphRun,
    font_cache: &FontCache,
) -> Option<Rect> {
    if run.glyphs.is_empty()
        || !run.transform.is_finite()
        || !run.font_size.is_finite()
        || !run.h_scale.is_finite()
    {
        return None;
    }
    let font = font_cache.get(run.font_id)?;
    let is_vertical = font.cid_cmap.as_ref().is_some_and(|c| c.wmode == 1);
    let fs = run.font_size.abs();
    let desc = 0.25 * fs;
    let mut min_x = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    if is_vertical {
        let signed_scale_x = run.font_size * run.h_scale;
        if !signed_scale_x.is_finite() || signed_scale_x == 0.0 {
            return None;
        }
        let div = font.advance_divisor();
        let abs_h_scale = run.h_scale.abs();
        for g in &run.glyphs {
            let mut w_mag = fs * abs_h_scale;
            if div.is_finite() && div > 0.0 {
                let adv_gid = font.glyph_advance(g.glyph_id);
                let adv_code = u16::try_from(g.code)
                    .map(|c| font.glyph_advance(c))
                    .unwrap_or(0.0);
                let adv_units = adv_gid.max(adv_code);
                if adv_units.is_finite() && adv_units > 0.0 {
                    w_mag = w_mag.max((adv_units / div) as f32 * fs * abs_h_scale);
                }
            }
            if g.x * signed_scale_x < 0.0 {
                w_mag = w_mag.max(2.0 * g.x.abs());
            }
            let signed_w = w_mag.copysign(signed_scale_x);
            let gx0 = g.x.min(g.x + signed_w);
            let gx1 = g.x.max(g.x + signed_w);
            let bot_y = g.y.min(g.y + run.font_size).min(g.y + g.advance);
            let top_y = g.y.max(g.y + run.font_size).max(g.y + g.advance);
            let gy0 = if run.font_size >= 0.0 {
                bot_y - desc
            } else {
                bot_y
            };
            let gy1 = if run.font_size >= 0.0 {
                top_y
            } else {
                top_y + desc
            };
            min_x = min_x.min(gx0);
            max_x = max_x.max(gx1);
            min_y = min_y.min(gy0);
            max_y = max_y.max(gy1);
        }
    } else {
        for g in &run.glyphs {
            if !g.advance.is_finite() || g.advance == 0.0 {
                return None;
            }
            let gx0 = g.x.min(g.x + g.advance);
            let gx1 = g.x.max(g.x + g.advance);
            let gy0 = g.y.min(g.y + run.font_size) - desc;
            let gy1 = g.y.max(g.y + run.font_size);
            min_x = min_x.min(gx0);
            max_x = max_x.max(gx1);
            min_y = min_y.min(gy0);
            max_y = max_y.max(gy1);
        }
    }
    if !(min_x.is_finite() && max_x.is_finite() && min_y.is_finite() && max_y.is_finite()) {
        return None;
    }
    let mut x0 = f64::INFINITY;
    let mut y0 = f64::INFINITY;
    let mut x1 = f64::NEG_INFINITY;
    let mut y1 = f64::NEG_INFINITY;
    for (lx, ly) in [
        (min_x as f64, min_y as f64),
        (max_x as f64, min_y as f64),
        (min_x as f64, max_y as f64),
        (max_x as f64, max_y as f64),
    ] {
        let p = zpdf_core::Point::new(lx, ly).transform(&run.transform);
        if !(p.x.is_finite() && p.y.is_finite()) {
            return None;
        }
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    Some(Rect::new(x0, y0, x1, y1))
}

fn glyph_run_is_opaque_black(run: &zpdf_display_list::GlyphRun) -> bool {
    effective_paint_alpha(&run.paint, run.alpha) >= 1.0 - 1e-3
        && matches!(
            run.paint,
            zpdf_display_list::Paint::Solid(c) if color_to_rgb_u8(&c) == (0, 0, 0)
        )
        && run.overprint.is_none_or(|op| {
            (op.active & zpdf_display_list::Overprint::K) != 0 && op.cmyk[3] >= 1.0 - 1e-3
        })
}

fn glyph_run_is_opaque_white(run: &zpdf_display_list::GlyphRun) -> bool {
    effective_paint_alpha(&run.paint, run.alpha) >= 1.0 - 1e-3
        && matches!(
            run.paint,
            zpdf_display_list::Paint::Solid(c) if color_to_rgb_u8(&c) == (255, 255, 255)
        )
        && run.overprint.is_none()
}

fn is_visible_native_only_glyph_run(cmd: &RenderCommand, font_cache: &FontCache) -> bool {
    let RenderCommand::DrawGlyphRun(run) = cmd else {
        return false;
    };
    if run.glyphs.is_empty()
        || effective_paint_alpha(&run.paint, run.alpha) <= 0.0
        || run.overprint.is_some_and(|op| op.active == 0)
    {
        return false;
    }
    font_cache
        .get(run.font_id)
        .is_some_and(|f| !f.has_font_data())
}

fn is_visible_paint_command(cmd: &RenderCommand, font_cache: &FontCache) -> bool {
    match cmd {
        RenderCommand::FillPath {
            paint,
            alpha,
            overprint,
            ..
        }
        | RenderCommand::StrokePath {
            paint,
            alpha,
            overprint,
            ..
        } => {
            effective_paint_alpha(paint, *alpha) > 0.0
                && !overprint.is_some_and(|op| op.active == 0)
        }
        RenderCommand::DrawGlyphRun(run) => {
            !run.glyphs.is_empty()
                && effective_paint_alpha(&run.paint, run.alpha) > 0.0
                && !run.overprint.is_some_and(|op| op.active == 0)
                && font_cache
                    .get(run.font_id)
                    .is_some_and(|f| f.has_font_data())
        }
        RenderCommand::DrawImage(d) => d.alpha.is_finite() && d.alpha > 0.0,
        RenderCommand::DrawInlineImage(d) => d.alpha.is_finite() && d.alpha > 0.0,
        _ => false,
    }
}

fn opaque_normal_fill_cover_rect(
    cmd: &RenderCommand,
    clip_stack: &[&RenderCommand],
) -> Option<Rect> {
    let RenderCommand::FillPath {
        path,
        paint,
        alpha,
        overprint,
        ..
    } = cmd
    else {
        return None;
    };
    if overprint.is_some()
        || !matches!(paint, zpdf_display_list::Paint::Solid(_))
        || effective_paint_alpha(paint, *alpha) < 1.0 - 1e-3
    {
        return None;
    }
    let rect = exact_axis_aligned_rect(path)?;
    match active_clip_rect_for(clip_stack) {
        ActiveClip::None => Some(rect),
        ActiveClip::Rect(Some(clip)) => intersect_rect(rect, clip),
        ActiveClip::Rect(None) | ActiveClip::Complex => None,
    }
}

/// Whether every pixel painted by `cmds` onto an opaque black `(0, 0, 0)`
/// backdrop pixel (`K = 1` in naïve CMYK) stays `(0, 0, 0)`.
fn group_preserves_opaque_black(cmds: &[RenderCommand], font_cache: &FontCache) -> bool {
    let mut preserving_blend_depth = 0usize;
    for cmd in cmds {
        match cmd {
            RenderCommand::PushBlendGroup {
                blend_mode,
                isolated,
                knockout,
                alpha,
                mask,
                ..
            } => {
                if preserving_blend_depth > 0 {
                    preserving_blend_depth += 1;
                } else {
                    let group_alpha = if alpha.is_finite() {
                        alpha.clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    if matches!(
                        blend_mode,
                        zpdf_display_list::BlendMode::Multiply
                            | zpdf_display_list::BlendMode::Darken
                    ) {
                        preserving_blend_depth = 1;
                    } else if *blend_mode != zpdf_display_list::BlendMode::Normal
                        || *isolated
                        || *knockout
                        || group_alpha < 1.0
                        || mask.is_some()
                    {
                        return false;
                    }
                }
            }
            RenderCommand::PopBlendGroup => {
                preserving_blend_depth = preserving_blend_depth.saturating_sub(1);
            }
            _ if preserving_blend_depth > 0 => {}
            RenderCommand::FillPath {
                paint,
                alpha,
                overprint,
                ..
            }
            | RenderCommand::StrokePath {
                paint,
                alpha,
                overprint,
                ..
            } => {
                if effective_paint_alpha(paint, *alpha) <= 0.0
                    || overprint.is_some_and(|op| op.active == 0)
                {
                    continue;
                }
                let Some(op) = overprint else {
                    return false;
                };
                if (op.active & zpdf_display_list::Overprint::K) != 0 && op.cmyk[3] < 1.0 - 1e-3 {
                    return false;
                }
            }
            RenderCommand::DrawGlyphRun(run) => {
                if effective_paint_alpha(&run.paint, run.alpha) <= 0.0
                    || run.overprint.is_some_and(|op| op.active == 0)
                    || !font_cache
                        .get(run.font_id)
                        .is_some_and(|f| f.has_font_data())
                {
                    continue;
                }
                let Some(op) = run.overprint else {
                    return false;
                };
                if (op.active & zpdf_display_list::Overprint::K) != 0 && op.cmyk[3] < 1.0 - 1e-3 {
                    return false;
                }
            }
            RenderCommand::DrawImage(d) => {
                if d.alpha.is_finite() && d.alpha > 0.0 {
                    return false;
                }
            }
            RenderCommand::DrawInlineImage(d) => {
                if d.alpha.is_finite() && d.alpha > 0.0 {
                    return false;
                }
            }
            RenderCommand::PushClip { .. }
            | RenderCommand::PushClipStroke { .. }
            | RenderCommand::PopClip => {}
        }
    }
    true
}

/// Whether every pixel painted by `cmds` onto an opaque white `(255, 255, 255)`
/// backdrop pixel stays `(255, 255, 255)`.
fn group_preserves_opaque_white(cmds: &[RenderCommand], font_cache: &FontCache) -> bool {
    let mut preserving_blend_depth = 0usize;
    for cmd in cmds {
        match cmd {
            RenderCommand::PushBlendGroup {
                blend_mode,
                isolated,
                knockout,
                alpha,
                mask,
                ..
            } => {
                if preserving_blend_depth > 0 {
                    preserving_blend_depth += 1;
                } else {
                    let group_alpha = if alpha.is_finite() {
                        alpha.clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    if matches!(
                        blend_mode,
                        zpdf_display_list::BlendMode::Screen
                            | zpdf_display_list::BlendMode::Lighten
                    ) {
                        preserving_blend_depth = 1;
                    } else if *blend_mode != zpdf_display_list::BlendMode::Normal
                        || *isolated
                        || *knockout
                        || group_alpha < 1.0
                        || mask.is_some()
                    {
                        return false;
                    }
                }
            }
            RenderCommand::PopBlendGroup => {
                preserving_blend_depth = preserving_blend_depth.saturating_sub(1);
            }
            _ if preserving_blend_depth > 0 => {}
            RenderCommand::FillPath {
                paint,
                alpha,
                overprint,
                ..
            }
            | RenderCommand::StrokePath {
                paint,
                alpha,
                overprint,
                ..
            } => {
                if effective_paint_alpha(paint, *alpha) <= 0.0
                    || overprint.is_some_and(|op| op.active == 0)
                {
                    continue;
                }
                let Some(op) = overprint else {
                    return false;
                };
                if (0..4).any(|i| op.paints(i) && op.cmyk[i] > 1e-3) {
                    return false;
                }
            }
            RenderCommand::DrawGlyphRun(run) => {
                if effective_paint_alpha(&run.paint, run.alpha) <= 0.0
                    || run.overprint.is_some_and(|op| op.active == 0)
                    || !font_cache
                        .get(run.font_id)
                        .is_some_and(|f| f.has_font_data())
                {
                    continue;
                }
                let Some(op) = run.overprint else {
                    return false;
                };
                if (0..4).any(|i| op.paints(i) && op.cmyk[i] > 1e-3) {
                    return false;
                }
            }
            RenderCommand::DrawImage(d) => {
                if d.alpha.is_finite() && d.alpha > 0.0 {
                    return false;
                }
            }
            RenderCommand::DrawInlineImage(d) => {
                if d.alpha.is_finite() && d.alpha > 0.0 {
                    return false;
                }
            }
            RenderCommand::PushClip { .. }
            | RenderCommand::PushClipStroke { .. }
            | RenderCommand::PopClip => {}
        }
    }
    true
}

fn pass1_touches_rect(
    rendered: &zpdf_render_cpu::RenderedPage,
    sub_render_rect: Rect,
    scale: f32,
    target: Option<Rect>,
) -> bool {
    let Some(target) = target else {
        return rendered.data.as_chunks::<4>().0.iter().any(|px| px[3] > 0);
    };
    let Some(inter) = intersect_rect(sub_render_rect, target) else {
        return false;
    };
    let s = scale as f64;
    if !(s.is_finite() && s > 0.0) || rendered.width == 0 || rendered.height == 0 {
        return false;
    }
    let w = rendered.width;
    let h = rendered.height;
    let stride = w as usize;
    if rendered.data.len() < stride.saturating_mul(h as usize).saturating_mul(4) {
        return true;
    }
    let x0 = (((inter.x0 - sub_render_rect.x0) * s).floor().max(0.0) as u32).min(w);
    let y0 = (((sub_render_rect.y1 - inter.y1) * s).floor().max(0.0) as u32).min(h);
    let x1 = (((inter.x1 - sub_render_rect.x0) * s).ceil().max(0.0) as u32).min(w);
    let y1 = (((sub_render_rect.y1 - inter.y0) * s).ceil().max(0.0) as u32).min(h);
    if x1 <= x0 || y1 <= y0 {
        return false;
    }
    for y in y0 as usize..y1 as usize {
        let row = y * stride;
        for x in x0 as usize..x1 as usize {
            if rendered.data[(row + x) * 4 + 3] > 0 {
                return true;
            }
        }
    }
    false
}

fn color_to_rgb_u8(c: &zpdf_display_list::Color) -> (u8, u8, u8) {
    let ch = |v: f32| {
        if v.is_finite() {
            (v.clamp(0.0, 1.0) * 255.0).round() as u8
        } else {
            0
        }
    };
    (ch(c.r), ch(c.g), ch(c.b))
}

fn effective_paint_alpha(paint: &zpdf_display_list::Paint, alpha: f32) -> f32 {
    if !alpha.is_finite() {
        return 0.0;
    }
    let paint_alpha = match paint {
        zpdf_display_list::Paint::Solid(c) if c.a.is_finite() => c.a,
        zpdf_display_list::Paint::Solid(_) => 0.0,
        _ => 1.0,
    };
    (alpha * paint_alpha).clamp(0.0, 1.0)
}

fn path_has_multiple_subpaths(path: &zpdf_display_list::Path) -> bool {
    let mut subpaths = 0usize;
    let mut in_subpath = false;
    for elem in path.iter() {
        match elem {
            zpdf_display_list::PathElement::MoveTo(_) => {
                subpaths += 1;
                in_subpath = true;
            }
            zpdf_display_list::PathElement::LineTo(_)
            | zpdf_display_list::PathElement::CurveTo(_, _, _) => {
                if !in_subpath {
                    subpaths += 1;
                    in_subpath = true;
                }
            }
            zpdf_display_list::PathElement::Close => {
                in_subpath = false;
            }
        }
        if subpaths > 1 {
            return true;
        }
    }
    false
}

fn segments_intersect_or_touch(
    a0: zpdf_core::Point,
    a1: zpdf_core::Point,
    b0: zpdf_core::Point,
    b1: zpdf_core::Point,
) -> bool {
    let ax_min = a0.x.min(a1.x);
    let ax_max = a0.x.max(a1.x);
    let ay_min = a0.y.min(a1.y);
    let ay_max = a0.y.max(a1.y);
    let bx_min = b0.x.min(b1.x);
    let bx_max = b0.x.max(b1.x);
    let by_min = b0.y.min(b1.y);
    let by_max = b0.y.max(b1.y);
    if ax_max < bx_min || bx_max < ax_min || ay_max < by_min || by_max < ay_min {
        return false;
    }
    let cross = |o: zpdf_core::Point, p: zpdf_core::Point, q: zpdf_core::Point| {
        (p.x - o.x) * (q.y - o.y) - (p.y - o.y) * (q.x - o.x)
    };
    let d1 = cross(a0, a1, b0);
    let d2 = cross(a0, a1, b1);
    let d3 = cross(b0, b1, a0);
    let d4 = cross(b0, b1, a1);
    if ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0))
        && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
    {
        return true;
    }
    let on_seg = |p: zpdf_core::Point, q: zpdf_core::Point, r: zpdf_core::Point| {
        r.x >= p.x.min(q.x) - 1e-9
            && r.x <= p.x.max(q.x) + 1e-9
            && r.y >= p.y.min(q.y) - 1e-9
            && r.y <= p.y.max(q.y) + 1e-9
    };
    (d1.abs() <= 1e-9 && on_seg(a0, a1, b0))
        || (d2.abs() <= 1e-9 && on_seg(a0, a1, b1))
        || (d3.abs() <= 1e-9 && on_seg(b0, b1, a0))
        || (d4.abs() <= 1e-9 && on_seg(b0, b1, a1))
}

fn path_single_subpath_may_self_intersect(path: &zpdf_display_list::Path) -> bool {
    let mut edges: Vec<(zpdf_core::Point, zpdf_core::Point)> = Vec::new();
    let mut start: Option<zpdf_core::Point> = None;
    let mut cur: Option<zpdf_core::Point> = None;
    for elem in path.iter() {
        match elem {
            zpdf_display_list::PathElement::MoveTo(p) => {
                start = Some(p);
                cur = Some(p);
            }
            zpdf_display_list::PathElement::LineTo(p) => {
                if let Some(prev) = cur {
                    if (prev.x - p.x).abs() > 1e-9 || (prev.y - p.y).abs() > 1e-9 {
                        edges.push((prev, p));
                    }
                } else {
                    start = Some(p);
                }
                cur = Some(p);
            }
            zpdf_display_list::PathElement::CurveTo(_, _, _) => {
                // Curved artwork outlines can self-intersect or depend on winding.
                return true;
            }
            zpdf_display_list::PathElement::Close => {
                if let (Some(prev), Some(st)) = (cur, start) {
                    if (prev.x - st.x).abs() > 1e-9 || (prev.y - st.y).abs() > 1e-9 {
                        edges.push((prev, st));
                    }
                }
                cur = start;
            }
        }
        if edges.len() > 64 {
            return true;
        }
    }
    if let (Some(prev), Some(st)) = (cur, start) {
        if (prev.x - st.x).abs() > 1e-9 || (prev.y - st.y).abs() > 1e-9 {
            edges.push((prev, st));
        }
    }
    let n = edges.len();
    if n < 4 {
        return false;
    }
    for i in 0..n {
        for j in (i + 2)..n {
            if i == 0 && j == n - 1 {
                continue;
            }
            if segments_intersect_or_touch(edges[i].0, edges[i].1, edges[j].0, edges[j].1) {
                return true;
            }
        }
    }
    false
}

/// Whether an artwork `FillPath` must use the CPU raster fallback to preserve
/// its exact PDF fill rule (`NonZero` vs `EvenOdd`) or non-solid paint, because
/// DrawingML `<a:custGeom><a:path>` has no fill-rule attribute.
fn artwork_fill_needs_raster(
    path: &zpdf_display_list::Path,
    _rule: zpdf_display_list::FillRule,
    paint: &zpdf_display_list::Paint,
) -> bool {
    !matches!(paint, zpdf_display_list::Paint::Solid(_))
        || path_has_multiple_subpaths(path)
        || path_single_subpath_may_self_intersect(path)
}

/// Whether an artwork `StrokePath` must use the CPU raster fallback to preserve
/// stroke geometry not carried by native PowerPoint `Line`/`FreeformPath` XML
/// (dashes, non-solid paint, round/square caps, or joins/miter limits on
/// multi-segment or curved paths).
fn artwork_stroke_needs_raster(
    path: &zpdf_display_list::Path,
    style: &zpdf_display_list::StrokeStyle,
    paint: &zpdf_display_list::Paint,
) -> bool {
    if !matches!(paint, zpdf_display_list::Paint::Solid(_)) {
        return true;
    }
    let has_dash = style
        .dash
        .as_ref()
        .is_some_and(|d| !d.array.is_empty() && d.array.iter().any(|&v| v > 0.0));
    if has_dash || style.cap != zpdf_display_list::LineCap::Butt {
        return true;
    }
    shape_recognizer::recognize_line(path).is_none()
}

fn path_control_bounds(path: &zpdf_display_list::Path) -> Option<Rect> {
    let mut x0 = f64::INFINITY;
    let mut y0 = f64::INFINITY;
    let mut x1 = f64::NEG_INFINITY;
    let mut y1 = f64::NEG_INFINITY;
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

const MAX_TRANSLATED_MASK_ENTRIES: usize = 64;
const MAX_TRANSLATED_MASK_BYTES: usize = 4 * 1024 * 1024;

fn estimate_display_list_bytes(dl: &DisplayList) -> usize {
    let mut bytes = std::mem::size_of::<DisplayList>()
        + dl.commands.len() * std::mem::size_of::<RenderCommand>();
    for cmd in &dl.commands {
        match cmd {
            RenderCommand::FillPath { path, .. }
            | RenderCommand::StrokePath { path, .. }
            | RenderCommand::PushClip { path, .. }
            | RenderCommand::PushClipStroke { path, .. } => {
                bytes = bytes.saturating_add(path.points().len().saturating_mul(24));
            }
            RenderCommand::DrawGlyphRun(run) => {
                bytes = bytes.saturating_add(
                    run.glyphs
                        .len()
                        .saturating_mul(std::mem::size_of::<zpdf_display_list::PositionedGlyph>()),
                );
            }
            _ => {}
        }
    }
    bytes
}

struct SoftMaskRebaser {
    origin_x: f64,
    origin_y: f64,
    scale_x: f64,
    scale_y: f64,
    /// Actual `CpuRenderer` page raster scale, used to preserve exact
    /// one-device-pixel hairlines independently of the curve-flattening clamp.
    render_scale: f32,
    /// Optional target surface rectangle in PDF user space (unit scale only).
    /// Masks whose unshifted geometry already fits inside `target_rect` do not
    /// need path translation so `CpuRenderer` can reuse one shared mask plane
    /// via `mask.offset` across repeated pattern tiles.
    target_rect: Option<Rect>,
    /// Memoized offset-presence check keyed by input `Arc<DisplayList>` address.
    offset_cache: std::collections::HashMap<usize, bool>,
    /// Memoized unshifted target-containment check keyed by input `Arc<DisplayList>` address.
    fits_target_cache: std::collections::HashMap<usize, bool>,
    /// Bounded memoized transformed display lists keyed by `(input Arc address, dx bits, dy bits)`.
    translated_cache: std::collections::HashMap<(usize, u64, u64), std::sync::Arc<DisplayList>>,
    retained_bytes: usize,
    /// Set whenever a translated mask was returned without being retained in
    /// `translated_cache`, instructing a long-lived `CpuRenderer` to clear its
    /// `Arc::as_ptr`-keyed `soft_mask_planes` cache before dropping the transient `Arc`.
    needs_cpu_mask_cache_invalidation: bool,
}

impl SoftMaskRebaser {
    fn new(origin_x: f64, origin_y: f64, scale_x: f64, scale_y: f64, render_scale: f32) -> Self {
        Self {
            origin_x,
            origin_y,
            scale_x,
            scale_y,
            render_scale,
            target_rect: None,
            offset_cache: std::collections::HashMap::new(),
            fits_target_cache: std::collections::HashMap::new(),
            translated_cache: std::collections::HashMap::new(),
            retained_bytes: 0,
            needs_cpu_mask_cache_invalidation: false,
        }
    }

    fn with_target_rect(mut self, target_rect: Rect) -> Self {
        self.target_rect = Some(target_rect);
        self
    }

    fn take_needs_cpu_mask_cache_invalidation(&mut self) -> bool {
        std::mem::replace(&mut self.needs_cpu_mask_cache_invalidation, false)
    }

    fn is_unit_scale(&self) -> bool {
        self.scale_x == 1.0 && self.scale_y == 1.0
    }

    fn is_uniform_scale(&self) -> bool {
        (self.scale_x - self.scale_y).abs() <= 1e-9
    }

    fn curve_resolution_scale(&self) -> f32 {
        (self.render_scale as f64 * self.scale_x.max(self.scale_y)).max(0.25) as f32
    }

    fn is_device_hairline(&self, style: &zpdf_display_list::StrokeStyle) -> bool {
        let device_max_scale = self.render_scale as f64 * self.scale_x.max(self.scale_y);
        style.width <= 0.0 || (style.width as f64 * device_max_scale) <= 1.0
    }

    fn min_hairline_user_width(&self) -> f32 {
        let device_min_scale = self.render_scale as f64 * self.scale_x.min(self.scale_y);
        if device_min_scale > 0.0 {
            (1.0 / device_min_scale) as f32
        } else {
            0.0
        }
    }

    fn canonical_bits(v: f64) -> u64 {
        if v == 0.0 {
            0.0f64.to_bits()
        } else {
            v.to_bits()
        }
    }

    fn map_point(&self, p: zpdf_core::Point, dx: f64, dy: f64) -> zpdf_core::Point {
        zpdf_core::Point::new(
            self.origin_x + (p.x + dx - self.origin_x) * self.scale_x,
            self.origin_y + (p.y + dy - self.origin_y) * self.scale_y,
        )
    }

    fn map_path(
        &self,
        path: &zpdf_display_list::Path,
        dx: f64,
        dy: f64,
    ) -> zpdf_display_list::Path {
        use zpdf_display_list::PathElement;

        let mut out = zpdf_display_list::Path::new();
        for elem in path.iter() {
            match elem {
                PathElement::MoveTo(p) => out.move_to(self.map_point(p, dx, dy)),
                PathElement::LineTo(p) => out.line_to(self.map_point(p, dx, dy)),
                PathElement::CurveTo(c1, c2, end) => out.curve_to(
                    self.map_point(c1, dx, dy),
                    self.map_point(c2, dx, dy),
                    self.map_point(end, dx, dy),
                ),
                PathElement::Close => out.close(),
            }
        }
        out
    }

    fn map_matrix(&self, m: zpdf_core::Matrix, dx: f64, dy: f64) -> zpdf_core::Matrix {
        zpdf_core::Matrix::new(
            m.a * self.scale_x,
            m.b * self.scale_y,
            m.c * self.scale_x,
            m.d * self.scale_y,
            self.origin_x + (m.e + dx - self.origin_x) * self.scale_x,
            self.origin_y + (m.f + dy - self.origin_y) * self.scale_y,
        )
    }

    fn map_rect(&self, r: Rect, dx: f64, dy: f64) -> Rect {
        let p0 = self.map_point(zpdf_core::Point::new(r.x0, r.y0), dx, dy);
        let p1 = self.map_point(zpdf_core::Point::new(r.x1, r.y1), dx, dy);
        Rect::new(p0.x, p0.y, p1.x, p1.y).normalize()
    }

    fn map_uniform_stroke_style(
        &self,
        style: &zpdf_display_list::StrokeStyle,
    ) -> zpdf_display_list::StrokeStyle {
        if self.is_unit_scale() {
            return style.clone();
        }
        let s = self.scale_x as f32;
        let mut out = style.clone();
        out.width *= s;
        if let Some(dash) = &mut out.dash {
            for v in &mut dash.array {
                *v *= s;
            }
            dash.phase *= s;
        }
        out
    }

    fn has_offset_in_commands(&mut self, commands: &[RenderCommand]) -> bool {
        for cmd in commands {
            if let RenderCommand::PushBlendGroup {
                mask: Some(mask), ..
            } = cmd
            {
                if mask.offset != (0.0, 0.0) || self.has_offset_in_list(&mask.commands) {
                    return true;
                }
            }
        }
        false
    }

    fn has_offset_in_list(&mut self, dl: &std::sync::Arc<DisplayList>) -> bool {
        let key = std::sync::Arc::as_ptr(dl) as usize;
        if let Some(&cached) = self.offset_cache.get(&key) {
            return cached;
        }
        let has = self.has_offset_in_commands(&dl.commands);
        self.offset_cache.insert(key, has);
        has
    }

    fn is_device_pixel_aligned(&self, offset: (f32, f32)) -> bool {
        let sx = offset.0 as f64 * self.render_scale as f64;
        let sy = offset.1 as f64 * self.render_scale as f64;
        sx.is_finite()
            && sy.is_finite()
            && (sx - sx.round()).abs() <= 1e-4
            && (sy - sy.round()).abs() <= 1e-4
    }

    fn mask_fits_in_target_and_aligned(&mut self, dl: &std::sync::Arc<DisplayList>) -> bool {
        let Some(target) = self.target_rect else {
            return false;
        };
        let key = std::sync::Arc::as_ptr(dl) as usize;
        if let Some(&cached) = self.fits_target_cache.get(&key) {
            return cached;
        }
        let mut fits = true;
        for cmd in &dl.commands {
            match cmd {
                RenderCommand::FillPath { path, .. } => {
                    if !path.is_empty() {
                        let Some(b) = path_control_bounds(path) else {
                            fits = false;
                            break;
                        };
                        if !rect_contains_with_eps(target, b, 1e-4) {
                            fits = false;
                            break;
                        }
                    }
                }
                RenderCommand::StrokePath { path, style, .. } => {
                    if !path.is_empty() {
                        let (Some(b), Some(grow)) =
                            (path_control_bounds(path), stroke_style_grow(style))
                        else {
                            fits = false;
                            break;
                        };
                        let expanded =
                            Rect::new(b.x0 - grow, b.y0 - grow, b.x1 + grow, b.y1 + grow);
                        if !rect_contains_with_eps(target, expanded, 1e-4) {
                            fits = false;
                            break;
                        }
                    }
                }
                RenderCommand::DrawImage(d) => {
                    for &(u, v) in &[(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
                        let p = zpdf_core::Point::new(u, v).transform(&d.transform);
                        if !(p.x.is_finite()
                            && p.y.is_finite()
                            && p.x >= target.x0 - 1e-4
                            && p.x <= target.x1 + 1e-4
                            && p.y >= target.y0 - 1e-4
                            && p.y <= target.y1 + 1e-4)
                        {
                            fits = false;
                            break;
                        }
                    }
                    if !fits {
                        break;
                    }
                }
                RenderCommand::DrawInlineImage(d) => {
                    for &(u, v) in &[(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
                        let p = zpdf_core::Point::new(u, v).transform(&d.transform);
                        if !(p.x.is_finite()
                            && p.y.is_finite()
                            && p.x >= target.x0 - 1e-4
                            && p.x <= target.x1 + 1e-4
                            && p.y >= target.y0 - 1e-4
                            && p.y <= target.y1 + 1e-4)
                        {
                            fits = false;
                            break;
                        }
                    }
                    if !fits {
                        break;
                    }
                }
                RenderCommand::DrawGlyphRun(_) => {
                    fits = false;
                    break;
                }
                RenderCommand::PushBlendGroup {
                    mask: Some(inner), ..
                } if !self.is_device_pixel_aligned(inner.offset)
                    || !self.mask_fits_in_target_and_aligned(&inner.commands) =>
                {
                    fits = false;
                    break;
                }
                _ => {}
            }
        }
        self.fits_target_cache.insert(key, fits);
        fits
    }

    fn rebase_mask(
        &mut self,
        mask: &zpdf_display_list::SoftMask,
        extra_dx: f64,
        extra_dy: f64,
    ) -> (zpdf_display_list::SoftMask, bool) {
        let dx = mask.offset.0 as f64 + extra_dx;
        let dy = mask.offset.1 as f64 + extra_dy;
        if self.is_unit_scale() {
            if dx == 0.0 && dy == 0.0 && !self.has_offset_in_list(&mask.commands) {
                let mut reused = mask.clone();
                reused.offset = (0.0, 0.0);
                return (reused, true);
            }
            // Reuse CpuRenderer's native shared mask plane only when every
            // shift (outer and nested) is an exact whole device pixel so
            // fractional sub-pixel shifts render with identical edge coverage
            // across standalone and backdrop passes.
            if extra_dx == 0.0
                && extra_dy == 0.0
                && self.is_device_pixel_aligned(mask.offset)
                && self.mask_fits_in_target_and_aligned(&mask.commands)
            {
                return (mask.clone(), true);
            }
        }
        let (commands, admitted) = self.translate_display_list(&mask.commands, dx, dy);
        (
            zpdf_display_list::SoftMask {
                kind: mask.kind,
                commands,
                offset: (0.0, 0.0),
                backdrop_luma: mask.backdrop_luma,
                transfer: mask.transfer.clone(),
            },
            admitted,
        )
    }

    fn translate_display_list(
        &mut self,
        dl: &std::sync::Arc<DisplayList>,
        dx: f64,
        dy: f64,
    ) -> (std::sync::Arc<DisplayList>, bool) {
        let key = (
            std::sync::Arc::as_ptr(dl) as usize,
            Self::canonical_bits(dx),
            Self::canonical_bits(dy),
        );
        if let Some(cached) = self.translated_cache.get(&key) {
            return (std::sync::Arc::clone(cached), true);
        }
        let mut out = DisplayList::new(self.map_rect(dl.page_rect, dx, dy));
        let mut all_children_admitted = true;
        for cmd in &dl.commands {
            let (translated, child_admitted) = self.translate_render_command(cmd, dx, dy);
            all_children_admitted &= child_admitted;
            out.push(translated);
        }
        let entry_bytes = estimate_display_list_bytes(&out);
        let arc = std::sync::Arc::new(out);
        // Only admit this display list if every translated child mask it owns
        // was also admitted and charged to `retained_bytes`; otherwise a small
        // wrapper would smuggle oversized children past the retention budget.
        if all_children_admitted
            && self.translated_cache.len() < MAX_TRANSLATED_MASK_ENTRIES
            && self.retained_bytes.saturating_add(entry_bytes) <= MAX_TRANSLATED_MASK_BYTES
        {
            self.retained_bytes = self.retained_bytes.saturating_add(entry_bytes);
            self.translated_cache
                .insert(key, std::sync::Arc::clone(&arc));
            (arc, true)
        } else {
            self.needs_cpu_mask_cache_invalidation = true;
            (arc, false)
        }
    }

    fn map_paint(&self, paint: &Paint, dx: f64, dy: f64) -> Paint {
        match paint {
            Paint::Tiling(tiling) => {
                Paint::Tiling(std::sync::Arc::new(zpdf_display_list::TilingPattern {
                    commands: std::sync::Arc::clone(&tiling.commands),
                    bbox: tiling.bbox,
                    x_step: tiling.x_step,
                    y_step: tiling.y_step,
                    matrix: self.map_matrix(tiling.matrix, dx, dy),
                    tiling_type: tiling.tiling_type,
                }))
            }
            other => other.clone(),
        }
    }

    fn translate_render_command(
        &mut self,
        cmd: &RenderCommand,
        dx: f64,
        dy: f64,
    ) -> (RenderCommand, bool) {
        match cmd {
            RenderCommand::FillPath {
                path,
                rule,
                paint,
                alpha,
                overprint,
            } => (
                RenderCommand::FillPath {
                    path: self.map_path(path, dx, dy),
                    rule: *rule,
                    paint: self.map_paint(paint, dx, dy),
                    alpha: *alpha,
                    overprint: *overprint,
                },
                true,
            ),
            RenderCommand::StrokePath {
                path,
                style,
                paint,
                alpha,
                overprint,
            } => (
                if self.is_uniform_scale() {
                    RenderCommand::StrokePath {
                        path: self.map_path(path, dx, dy),
                        style: self.map_uniform_stroke_style(style),
                        paint: self.map_paint(paint, dx, dy),
                        alpha: *alpha,
                        overprint: *overprint,
                    }
                } else if self.is_device_hairline(style) {
                    // Hairlines must remain 1 full device pixel in every
                    // direction after anisotropic fitting: dash in user space
                    // so directional dash lengths stay exact, map the centerline
                    // to render space, and outline at `1 / render_scale` render
                    // units (exact 1 device pixel, independent of the curve
                    // flattening clamp and avoiding tiny-skia's <=2px hairline
                    // clip-inset early return).
                    let curve_res = self.curve_resolution_scale();
                    let dashed = if style.dash.is_some() {
                        Some(zpdf_render_cpu::CpuRenderer::dash_path(
                            path, style, curve_res,
                        ))
                    } else {
                        None
                    };
                    let mapped_centerline = self.map_path(dashed.as_ref().unwrap_or(path), dx, dy);
                    let mut hairline_style = style.clone();
                    hairline_style.width = 0.0;
                    hairline_style.dash = None;
                    let hairline_render_width = if self.render_scale > 0.0 {
                        1.0 / self.render_scale
                    } else {
                        0.0
                    };
                    let outlined = zpdf_render_cpu::CpuRenderer::stroke_path_to_outline(
                        &mapped_centerline,
                        &hairline_style,
                        hairline_render_width,
                        curve_res,
                    )
                    .unwrap_or_default();
                    RenderCommand::FillPath {
                        path: outlined,
                        rule: zpdf_display_list::FillRule::NonZero,
                        paint: self.map_paint(paint, dx, dy),
                        alpha: *alpha,
                        overprint: *overprint,
                    }
                } else {
                    // Under an anisotropic fit (e.g. X=1, Y=4/3), expand the
                    // stroke and dashes into a filled outline in user space
                    // first so directional widths and dash lengths transform
                    // with the exact Jacobian.
                    let outlined = zpdf_render_cpu::CpuRenderer::stroke_path_to_outline(
                        path,
                        style,
                        self.min_hairline_user_width(),
                        self.curve_resolution_scale(),
                    )
                    .map(|outline| self.map_path(&outline, dx, dy))
                    .unwrap_or_default();
                    RenderCommand::FillPath {
                        path: outlined,
                        rule: zpdf_display_list::FillRule::NonZero,
                        paint: self.map_paint(paint, dx, dy),
                        alpha: *alpha,
                        overprint: *overprint,
                    }
                },
                true,
            ),
            RenderCommand::DrawGlyphRun(run) => {
                let mut moved = run.clone();
                moved.transform = self.map_matrix(moved.transform, dx, dy);
                moved.paint = self.map_paint(&run.paint, dx, dy);
                (RenderCommand::DrawGlyphRun(moved), true)
            }
            RenderCommand::DrawImage(d) => {
                let mut moved = d.clone();
                moved.transform = self.map_matrix(moved.transform, dx, dy);
                (RenderCommand::DrawImage(moved), true)
            }
            RenderCommand::DrawInlineImage(d) => {
                let mut moved = d.clone();
                moved.transform = self.map_matrix(moved.transform, dx, dy);
                (RenderCommand::DrawInlineImage(moved), true)
            }
            RenderCommand::PushClip { path, rule } => (
                RenderCommand::PushClip {
                    path: self.map_path(path, dx, dy),
                    rule: *rule,
                },
                true,
            ),
            RenderCommand::PushClipStroke { path, style } => (
                if self.is_uniform_scale() {
                    RenderCommand::PushClipStroke {
                        path: self.map_path(path, dx, dy),
                        style: self.map_uniform_stroke_style(style),
                    }
                } else if self.is_device_hairline(style) {
                    let curve_res = self.curve_resolution_scale();
                    let dashed = if style.dash.is_some() {
                        Some(zpdf_render_cpu::CpuRenderer::dash_path(
                            path, style, curve_res,
                        ))
                    } else {
                        None
                    };
                    let mapped_centerline = self.map_path(dashed.as_ref().unwrap_or(path), dx, dy);
                    let mut hairline_style = style.clone();
                    hairline_style.width = 0.0;
                    hairline_style.dash = None;
                    let hairline_render_width = if self.render_scale > 0.0 {
                        1.0 / self.render_scale
                    } else {
                        0.0
                    };
                    let outlined = zpdf_render_cpu::CpuRenderer::stroke_path_to_outline(
                        &mapped_centerline,
                        &hairline_style,
                        hairline_render_width,
                        curve_res,
                    )
                    .unwrap_or_default();
                    RenderCommand::PushClip {
                        path: outlined,
                        rule: zpdf_display_list::FillRule::NonZero,
                    }
                } else {
                    let outlined = zpdf_render_cpu::CpuRenderer::stroke_path_to_outline(
                        path,
                        style,
                        self.min_hairline_user_width(),
                        self.curve_resolution_scale(),
                    )
                    .map(|outline| self.map_path(&outline, dx, dy))
                    .unwrap_or_default();
                    RenderCommand::PushClip {
                        path: outlined,
                        rule: zpdf_display_list::FillRule::NonZero,
                    }
                },
                true,
            ),
            RenderCommand::PopClip => (RenderCommand::PopClip, true),
            RenderCommand::PushBlendGroup {
                blend_mode,
                isolated,
                knockout,
                bounds,
                alpha,
                mask,
            } => {
                let (rebased_mask, admitted) = match mask {
                    Some(inner) => {
                        let (m, ok) = self.rebase_mask(inner, dx, dy);
                        (Some(m), ok)
                    }
                    None => (None, true),
                };
                (
                    RenderCommand::PushBlendGroup {
                        blend_mode: *blend_mode,
                        isolated: *isolated,
                        knockout: *knockout,
                        bounds: self.map_rect(*bounds, dx, dy),
                        alpha: *alpha,
                        mask: rebased_mask,
                    },
                    admitted,
                )
            }
            RenderCommand::PopBlendGroup => (RenderCommand::PopBlendGroup, true),
        }
    }

    fn prepare_command<'c>(
        &mut self,
        cmd: &'c RenderCommand,
    ) -> std::borrow::Cow<'c, RenderCommand> {
        if self.is_unit_scale() {
            match cmd {
                RenderCommand::PushBlendGroup {
                    blend_mode,
                    isolated,
                    knockout,
                    bounds,
                    alpha,
                    mask: Some(mask),
                } if mask.offset != (0.0, 0.0) || self.has_offset_in_list(&mask.commands) => {
                    let (rebased, _) = self.rebase_mask(mask, 0.0, 0.0);
                    std::borrow::Cow::Owned(RenderCommand::PushBlendGroup {
                        blend_mode: *blend_mode,
                        isolated: *isolated,
                        knockout: *knockout,
                        bounds: *bounds,
                        alpha: *alpha,
                        mask: Some(rebased),
                    })
                }
                _ => std::borrow::Cow::Borrowed(cmd),
            }
        } else {
            let (translated, _) = self.translate_render_command(cmd, 0.0, 0.0);
            std::borrow::Cow::Owned(translated)
        }
    }
}

#[cfg(test)]
fn rebase_soft_masks(commands: &[RenderCommand]) -> Vec<RenderCommand> {
    let mut rebaser = SoftMaskRebaser::new(0.0, 0.0, 1.0, 1.0, 1.0);
    commands
        .iter()
        .map(|cmd| rebaser.prepare_command(cmd).into_owned())
        .collect()
}

fn intersect_rect(a: Rect, b: Rect) -> Option<Rect> {
    let a = a.normalize();
    let b = b.normalize();
    let x0 = a.x0.max(b.x0);
    let y0 = a.y0.max(b.y0);
    let x1 = a.x1.min(b.x1);
    let y1 = a.y1.min(b.y1);
    (x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite() && x1 > x0 && y1 > y0)
        .then(|| Rect::new(x0, y0, x1, y1))
}

fn unpremultiply_rgba(data: &mut [u8]) {
    for px in data.as_chunks_mut::<4>().0 {
        let a = px[3] as u32;
        if a == 0 {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
        } else if a < 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

fn rect_contains_with_eps(outer: Rect, inner: Rect, eps: f64) -> bool {
    let outer = outer.normalize();
    let inner = inner.normalize();
    inner.x0 >= outer.x0 - eps
        && inner.y0 >= outer.y0 - eps
        && inner.x1 <= outer.x1 + eps
        && inner.y1 <= outer.y1 + eps
}

/// Whether the 2D linear part of `m` maps the unit X and Y axes to non-orthogonal
/// vectors (sheared parallelogram that `SlideElement::Transformed` cannot represent
/// with rotation + reflection alone).
fn matrix_has_shear(m: &zpdf_core::Matrix) -> bool {
    let len_u = m.a.hypot(m.b);
    let len_v = m.c.hypot(m.d);
    if !len_u.is_finite() || !len_v.is_finite() || len_u == 0.0 || len_v == 0.0 {
        return false;
    }
    let dot = (m.a * m.c + m.b * m.d).abs();
    dot / (len_u * len_v) > 1e-4
}

fn validate_rgba_buffer(width: u32, height: u32, data: &[u8]) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(zpdf_core::Error::StreamDecode(format!(
            "invalid RGBA image dimensions: {width}x{height}"
        )));
    }
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| {
            zpdf_core::Error::StreamDecode(format!(
                "RGBA image dimensions overflow: {width}x{height}"
            ))
        })?;
    if data.len() != expected {
        return Err(zpdf_core::Error::StreamDecode(format!(
            "RGBA image buffer length mismatch: expected {expected} bytes for {width}x{height}, got {}",
            data.len()
        )));
    }
    Ok(())
}

/// Convert RGBA8 pixels to PNG format
fn rgba_to_png(width: u32, height: u32, data: &[u8]) -> Result<Vec<u8>> {
    validate_rgba_buffer(width, height, data)?;
    let mut png_data = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut png_data);
    encoder
        .write_image(data, width, height, image::ExtendedColorType::Rgba8)
        .map_err(|e| zpdf_core::Error::StreamDecode(format!("PNG encode: {e}")))?;
    Ok(png_data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pt_to_emu_conversion() {
        assert_eq!(pt_to_emu(72.0), 914400); // 1 inch
        assert_eq!(pt_to_emu(0.0), 0);
    }
}

#[cfg(test)]
mod export_tests {
    use super::*;
    use zpdf_core::Matrix;
    use zpdf_display_list::{Color, DisplayList, GlyphRun, Paint, PositionedGlyph, RenderCommand};
    use zpdf_font::cmap::ToUnicodeMap;
    use zpdf_font::{FontCache, LoadedFont};

    fn positioned_slide(transforms: &[Matrix]) -> PptxSlide {
        let mut font = LoadedFont::new_placeholder("ABCDEF+Helvetica-BoldOblique".into());
        font.to_unicode = Some(ToUnicodeMap::parse(
            b"begincmap 1 beginbfchar <41> <0041> endbfchar endcmap",
        ));
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 120.0, 80.0));
        for &transform in transforms {
            dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
                font_id,
                font_size: 12.0,
                glyphs: vec![PositionedGlyph {
                    glyph_id: 1,
                    code_len: 1,
                    code: 65,
                    x: 0.0,
                    y: 0.0,
                    advance: 6.0,
                }],
                paint: Paint::Solid(Color::black()),
                alpha: 1.0,
                overprint: None,
                transform,
                h_scale: 1.0,
            }));
        }
        display_list_to_slide(
            &dl,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .unwrap()
    }

    #[test]
    fn text_boxes_keep_translation_scale_and_style() {
        let slide = positioned_slide(&[
            Matrix::new(2.0, 0.0, 0.0, 2.0, 20.0, 30.0),
            Matrix::translate(60.0, 20.0),
        ]);
        match &slide.elements[0] {
            SlideElement::TextBox {
                x_emu,
                y_emu,
                width_emu,
                height_emu,
                font_size_pt,
                font_family,
                bold,
                italic,
                ..
            } => {
                assert_eq!(
                    (*x_emu, *y_emu, *width_emu, *height_emu),
                    (254000, 330200, 152400, 304800)
                );
                assert_eq!(*font_size_pt, 24.0);
                assert_eq!(font_family, "Helvetica-BoldOblique");
                assert!(*bold && *italic);
            }
            _ => panic!("unrotated text"),
        }
        assert!(
            matches!(&slide.elements[1],SlideElement::TextBox{x_emu,y_emu,..} if (*x_emu,*y_emu)==(762000,609600))
        );
    }

    #[test]
    fn rotated_and_reflected_text_remains_editable_in_drawingml() {
        let slide = positioned_slide(&[
            Matrix::new(0.0, 1.0, -1.0, 0.0, 40.0, 20.0),
            Matrix::new(1.0, 0.0, 0.0, -1.0, 10.0, 30.0),
        ]);
        let presentation = PptxPresentation {
            slides: vec![slide],
            width_emu: 1524000,
            height_emu: 1016000,
        };
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(
            PptxWriter::new(presentation).write_to_bytes().unwrap(),
        ))
        .unwrap();
        let xml = String::from_utf8(zip_entry(&mut zip, "ppt/slides/slide1.xml")).unwrap();
        assert!(xml.contains("rot=\"16200000\""));
        assert!(xml.contains("flipV=\"1\""));
        assert_eq!(xml.matches("<a:t>A</a:t>").count(), 2);
        assert!(xml.contains("</a:rPr>"));
        assert!(!xml.contains("sz=\"1200\" b=\"1\" i=\"1\"/>"));
    }

    /// Text of every text box on a one-run slide drawn with `font`.
    fn run_text(font: LoadedFont, glyph_id: u16, code: u32) -> String {
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 612.0, 792.0));
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 12.0,
            glyphs: vec![PositionedGlyph {
                glyph_id,
                code_len: 0,
                code,
                x: 100.0,
                y: 100.0,
                advance: 6.0,
            }],
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
            transform: Matrix::identity(),
            h_scale: 1.0,
        }));
        let slide = display_list_to_slide(
            &dl,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide");
        slide
            .elements
            .iter()
            .filter_map(|e| match e {
                SlideElement::TextBox { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn composite_font_text_comes_from_to_unicode_by_code() {
        let font = || {
            let mut font = LoadedFont::new_placeholder("Cid".into());
            font.font_type = zpdf_font::PdfFontType::Type0CidType2;
            font.to_unicode = Some(ToUnicodeMap::parse(
                b"begincmap 1 begincodespacerange <0000> <FFFF> endcodespacerange \
                  1 beginbfchar <0102> <00E9> endbfchar endcmap",
            ));
            font
        };
        // Looking up the glyph id instead of the code finds nothing for the
        // first run and the mapped character for the second; an unmapped code
        // yields no text rather than the code read as a character.
        assert_eq!(run_text(font(), 7, 0x0102), "\u{e9}");
        assert_eq!(run_text(font(), 0x0102, 0x4E00), "");
    }

    #[test]
    fn composite_font_mixed_one_and_three_byte_to_unicode_exports_three_byte_code() {
        let make_font = || {
            let mut font = LoadedFont::new_placeholder("MixedThreeByte".into());
            font.font_type = zpdf_font::PdfFontType::Type0CidType2;
            font.to_unicode = Some(ToUnicodeMap::parse(
                b"begincmap 2 begincodespacerange <00> <FF> <000000> <FFFFFF> endcodespacerange \
                  2 beginbfchar <41> <0058> <010203> <0041> endbfchar endcmap",
            ));
            font
        };

        // Explicit source byte length (`code_len = 3` and `code_len = 1`) must
        // match `(raw_code, len)` directly before fixed-width 2-byte recovery
        // splits `<010203>` into `<0102>` and `<03>`.
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), make_font());
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 612.0, 792.0));
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 12.0,
            glyphs: vec![
                PositionedGlyph {
                    glyph_id: 7,
                    code_len: 3,
                    code: 0x010203,
                    x: 100.0,
                    y: 100.0,
                    advance: 6.0,
                },
                PositionedGlyph {
                    glyph_id: 8,
                    code_len: 1,
                    code: 0x41,
                    x: 106.0,
                    y: 100.0,
                    advance: 6.0,
                },
            ],
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
            transform: Matrix::identity(),
            h_scale: 1.0,
        }));
        let slide = display_list_to_slide(
            &dl,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide");
        let explicit_text: String = slide
            .elements
            .iter()
            .filter_map(|e| match e {
                SlideElement::TextBox { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(explicit_text, "AX");

        // Inferred-width path (`code_len = 0` via `run_text`) must also emit
        // `"A"` for `<010203>` and `"X"` for `<41>`.
        assert_eq!(run_text(make_font(), 7, 0x010203), "A");
        assert_eq!(run_text(make_font(), 8, 0x41), "X");

        // With `code_len = 0`, no `/ToUnicode`, and `/90ms-RKSJ-H`, `<41>` is a
        // complete 1-byte code and must not be zero-padded to `<0041>` (which
        // would decode `<00>` and drop `<41>`), while `<8141>` still infers
        // 2 bytes and `/UniGB-UCS2-H` still infers 2 bytes for `<0041>`.
        let rksj_font = || {
            let mut font = LoadedFont::new_placeholder("Rksj".into());
            font.font_type = zpdf_font::PdfFontType::Type0CidType2;
            font.cid_cmap = zpdf_font::cmap::CidCMap::predefined("90ms-RKSJ-H");
            font
        };
        assert_eq!(run_text(rksj_font(), 1, 0x41), "A");
        assert_eq!(run_text(rksj_font(), 2, 0x8141), "\u{3001}");

        let mut ucs2_font = LoadedFont::new_placeholder("Ucs2".into());
        ucs2_font.font_type = zpdf_font::PdfFontType::Type0CidType2;
        ucs2_font.cid_cmap = zpdf_font::cmap::CidCMap::predefined("UniGB-UCS2-H");
        assert_eq!(run_text(ucs2_font, 1, 0x41), "A");

        // Fixed-width 1-byte recovery still splits a 2-byte unmapped chunk
        // when no exact `(raw_code, len)` entry exists.
        let mut one_byte_font = LoadedFont::new_placeholder("OneByteFallback".into());
        one_byte_font.font_type = zpdf_font::PdfFontType::Type0CidType2;
        one_byte_font.to_unicode = Some(ToUnicodeMap::parse(
            b"begincmap 1 begincodespacerange <00> <FF> endcodespacerange \
              2 beginbfchar <57> <0057> <69> <4E2D> endbfchar endcmap",
        ));
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), one_byte_font);
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 612.0, 792.0));
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 12.0,
            glyphs: vec![PositionedGlyph {
                glyph_id: 1,
                code_len: 2,
                code: 0x5769,
                x: 100.0,
                y: 100.0,
                advance: 6.0,
            }],
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
            transform: Matrix::identity(),
            h_scale: 1.0,
        }));
        let slide = display_list_to_slide(
            &dl,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide");
        let fixed_text: String = slide
            .elements
            .iter()
            .filter_map(|e| match e {
                SlideElement::TextBox { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(fixed_text, "W中");
    }

    #[test]
    fn to_unicode_is_keyed_by_character_code_not_glyph_id() {
        let mut font = LoadedFont::new_placeholder("Simple".into());
        // Map code 0x41 to a non-ASCII character, so falling back to the code
        // itself ("A") cannot pass for a ToUnicode hit.
        font.to_unicode = Some(ToUnicodeMap::parse(
            b"begincmap 1 begincodespacerange <00> <FF> endcodespacerange \
              1 beginbfchar <41> <00E9> endbfchar endcmap",
        ));
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 612.0, 792.0));
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 12.0,
            glyphs: vec![PositionedGlyph {
                glyph_id: 99,
                code_len: 1,
                code: 0x41,
                x: 100.0,
                y: 100.0,
                advance: 6.0,
            }],
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
            transform: Matrix::identity(),
            h_scale: 1.0,
        }));

        let slide = display_list_to_slide(
            &dl,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide");
        let text: String = slide
            .elements
            .iter()
            .filter_map(|e| match e {
                SlideElement::TextBox { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "\u{e9}");
    }

    fn zip_entry(archive: &mut zip::ZipArchive<std::io::Cursor<Vec<u8>>>, name: &str) -> Vec<u8> {
        use std::io::Read;
        let mut entry = archive
            .by_name(name)
            .unwrap_or_else(|_| panic!("{name} missing"));
        let mut out = Vec::new();
        entry.read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn image_ids_are_renumbered_across_slides() {
        // Both slides number their picture "image1"; each carries different
        // bytes so a slide pointing at the other's media part is caught.
        let image = |data: &[u8]| PptxSlide {
            page_index: 0,
            elements: vec![SlideElement::Image {
                x_emu: 0,
                y_emu: 0,
                width_emu: 100,
                height_emu: 100,
                image_data: data.to_vec(),
                image_id: "image1".to_string(),
            }],
        };
        let presentation = PptxPresentation {
            slides: vec![image(b"first-slide"), image(b"second-slide")],
            width_emu: 914400,
            height_emu: 914400,
        };
        let bytes = PptxWriter::new(presentation)
            .write_to_bytes()
            .expect("write pptx");
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().unwrap().to_string())
            .collect();
        assert!(
            names.iter().any(|n| n == "ppt/media/image1.png"),
            "{names:?}"
        );
        assert!(
            names.iter().any(|n| n == "ppt/media/image2.png"),
            "{names:?}"
        );
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            names.len(),
            "duplicate ZIP members: {names:?}"
        );

        // Each slide's picture resolves, through its own relationship, to the
        // media part holding that slide's bytes.
        for (slide, data) in [(1, &b"first-slide"[..]), (2, &b"second-slide"[..])] {
            let xml = String::from_utf8(zip_entry(
                &mut archive,
                &format!("ppt/slides/slide{slide}.xml"),
            ))
            .unwrap();
            let rels = String::from_utf8(zip_entry(
                &mut archive,
                &format!("ppt/slides/_rels/slide{slide}.xml.rels"),
            ))
            .unwrap();
            let embed = xml
                .split("r:embed=\"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .expect("picture reference");
            let target = rels
                .split(&format!("Id=\"{embed}\""))
                .nth(1)
                .and_then(|rest| rest.split("Target=\"../").nth(1))
                .and_then(|rest| rest.split('"').next())
                .expect("relationship for the picture");
            assert_eq!(zip_entry(&mut archive, &format!("ppt/{target}")), data);
        }
    }

    #[test]
    fn freeform_paths_reach_the_slide_xml() {
        let presentation = PptxPresentation {
            slides: vec![PptxSlide {
                page_index: 0,
                elements: vec![SlideElement::FreeformPath {
                    path_data: "M 10 10 L 30 10 C 35 10 40 15 40 20 Z".to_string(),
                    fill_rgb: None,
                    stroke_rgb: Some((0, 0, 0)),
                    stroke_width_pt: 0.0,
                }],
            }],
            width_emu: 914400,
            height_emu: 914400,
        };
        let bytes = PptxWriter::new(presentation)
            .write_to_bytes()
            .expect("write pptx");
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
        let xml = String::from_utf8(zip_entry(&mut archive, "ppt/slides/slide1.xml")).unwrap();
        assert!(xml.contains("<a:custGeom>"), "{xml}");
        assert!(xml.contains("<a:cubicBezTo>"), "{xml}");
        // A zero-width PDF stroke is a hairline, not an invisible outline.
        assert!(!xml.contains("<a:ln><a:noFill/></a:ln>"), "{xml}");
        assert!(xml.contains("<a:ln w=\"3175\">"), "{xml}");
    }

    #[test]
    fn recovered_type0_two_byte_code_exports_slide_text_via_logical_font() {
        use std::sync::Arc;
        use zpdf_font::{CidWidths, FontProvider, LoadedFont, PdfFontType};

        const PRIMARY_TTF: &[u8] =
            include_bytes!("../../zpdf-font/tests/fixtures/coverage-primary.ttf");

        let probe = LoadedFont::new_with_data(
            PdfFontType::TrueType,
            "Probe".into(),
            PRIMARY_TTF.to_vec(),
            CidWidths::new(1000.0),
        );
        let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;
        let gid_b = probe.unicode_glyph('B' as u32).unwrap().0;

        // Corrupt only `gid_a` in `PRIMARY_TTF` (invalid `loca` start > end) so
        // the embedded Type 0 face stays primary and lazily admits a simple
        // TrueType descriptor-substitute drawing face for `<0102>` ('A').
        let mut partial_ttf = PRIMARY_TTF.to_vec();
        let num_tables = u16::from_be_bytes([partial_ttf[4], partial_ttf[5]]) as usize;
        let mut loca_off = None;
        for i in 0..num_tables {
            let rec = 12 + i * 16;
            if &partial_ttf[rec..rec + 4] == b"loca" {
                loca_off = Some(u32::from_be_bytes(
                    partial_ttf[rec + 8..rec + 12].try_into().unwrap(),
                ) as usize);
                break;
            }
        }
        let loca_off = loca_off.expect("loca table");
        let entry = loca_off + gid_a as usize * 4;
        partial_ttf[entry..entry + 4].copy_from_slice(&0xFFFF_FF00u32.to_be_bytes());

        let cmap = format!(
            "/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
             /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> def \
             /CMapName /TwoByteRecovered def /CMapType 1 def \
             1 begincodespacerange <0000> <FFFF> endcodespacerange \
             2 begincidchar <0102> {gid_a} <0103> {gid_b} endcidchar \
             endcmap CMapName currentdict /CMap defineresource pop end end"
        );
        let to_unicode = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
            1 begincodespacerange <0000> <FFFF> endcodespacerange \
            2 beginbfchar <0102> <0041> <0103> <0042> endbfchar \
            endcmap CMapName currentdict /CMap defineresource pop end end";

        let stream = |dict: &str, bytes: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", bytes.len()).into_bytes();
            out.extend_from_slice(bytes);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let objects: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] \
              /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
            stream("", b"0 0 0 rg BT /F1 18 Tf 20 50 Td <01020103> Tj ET"),
            b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+PptxRecoveredCid \
              /Encoding 9 0 R /DescendantFonts [6 0 R] /ToUnicode 10 0 R >>"
                .to_vec(),
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+PptxRecoveredCid \
              /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
              /CIDToGIDMap /Identity /FontDescriptor 7 0 R /DW 600 >>"
                .to_vec(),
            b"<< /Type /FontDescriptor /FontName /ABCDEF+PptxRecoveredCid /Flags 32 /FontFile2 8 0 R >>"
                .to_vec(),
            stream("", &partial_ttf),
            stream("", cmap.as_bytes()),
            stream("", to_unicode),
        ];
        let mut pdf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
        let mut offsets = Vec::with_capacity(objects.len());
        for (idx, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", idx + 1).as_bytes());
            pdf.extend_from_slice(body);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref = pdf.len();
        let count = objects.len() + 1;
        pdf.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
        for off in offsets {
            pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                .as_bytes(),
        );

        let provider = Arc::new(FontProvider::memory_only());
        provider
            .register_with_aliases(PRIMARY_TTF, 0, &["PptxRecoveredCid"])
            .unwrap();
        let doc = zpdf_document::PdfDocument::open(pdf)
            .unwrap()
            .with_font_provider(provider);
        let page = doc.page(0).unwrap();
        let mut fonts = doc.load_page_fonts(&page);
        let (content, _) = doc.page_content_with_diagnostics(&page).unwrap();
        let mut images = ImageCache::new();
        let interpreter = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_shared_fonts(&doc)
            .with_images(&mut images);
        let (display_list, _) = interpreter.interpret_with_stats(&content);

        let (fid_primary, primary) = fonts.get_by_name("F1").unwrap();
        assert!(!primary.is_substitute, "embedded Type 0 face stays primary");
        let runs: Vec<_> = display_list
            .commands
            .iter()
            .filter_map(|cmd| match cmd {
                RenderCommand::DrawGlyphRun(run) => Some(run),
                _ => None,
            })
            .collect();
        assert_eq!(runs.len(), 2, "recovered <0102> + embedded <0103>");
        assert_ne!(
            runs[0].font_id, fid_primary,
            "<0102> must draw on the admitted descriptor substitute drawing face"
        );
        assert!(matches!(
            fonts.get(runs[0].font_id).unwrap().font_type,
            PdfFontType::TrueType
        ));
        assert_eq!(runs[0].glyphs[0].code, 0x0102);

        let slide = display_list_to_slide(
            &display_list,
            0,
            &fonts,
            &images,
            &ConversionOptions::default(),
        )
        .expect("slide export");
        let slide_text: String = slide
            .elements
            .iter()
            .filter_map(|e| match e {
                SlideElement::TextBox { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            slide_text, "AB",
            "PPTX slide export must recover 'A' for two-byte code <0102> through logical_font"
        );
    }

    #[test]
    fn recovered_type0_unicode_cmap_and_utf16_surrogate_export_slide_text() {
        use std::sync::Arc;
        use zpdf_font::{CidWidths, FontProvider, LoadedFont, PdfFontType};

        const PRIMARY_TTF: &[u8] =
            include_bytes!("../../zpdf-font/tests/fixtures/coverage-primary.ttf");

        let probe = LoadedFont::new_with_data(
            PdfFontType::TrueType,
            "Probe".into(),
            PRIMARY_TTF.to_vec(),
            CidWidths::new(1000.0),
        );
        let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;

        // Corrupt only `gid_a` so the embedded Type 0 font stays primary and
        // lazily admits a drawing-only descriptor substitute for the damaged
        // glyph while keeping 'B' on the embedded face.
        let mut partial_ttf = PRIMARY_TTF.to_vec();
        let num_tables = u16::from_be_bytes([partial_ttf[4], partial_ttf[5]]) as usize;
        let mut loca_off = None;
        for i in 0..num_tables {
            let rec = 12 + i * 16;
            if &partial_ttf[rec..rec + 4] == b"loca" {
                loca_off = Some(u32::from_be_bytes(
                    partial_ttf[rec + 8..rec + 12].try_into().unwrap(),
                ) as usize);
                break;
            }
        }
        let loca_off = loca_off.expect("loca table");
        let entry = loca_off + gid_a as usize * 4;
        partial_ttf[entry..entry + 4].copy_from_slice(&0xFFFF_FF00u32.to_be_bytes());

        let stream = |dict: &str, bytes: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", bytes.len()).into_bytes();
            out.extend_from_slice(bytes);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (idx, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", idx + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            let count = objects.len() + 1;
            pdf.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                    .as_bytes(),
            );
            pdf
        };

        let provider = Arc::new(FontProvider::memory_only());
        provider
            .register_with_aliases(PRIMARY_TTF, 0, &["PptxUnicodeCmapFace"])
            .unwrap();

        // 1. `/UniGB-UCS2-H` WITHOUT `/ToUnicode`: recovered `<0041>` draws via
        //    descriptor substitute and must export slide text "AB" via the
        //    logical font's CMap decoder.
        let ucs2_pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] \
              /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
            stream("", b"0 0 0 rg BT /F1 18 Tf 20 50 Td <00410042> Tj ET"),
            b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+PptxUnicodeCmapFace \
              /Encoding /UniGB-UCS2-H /DescendantFonts [6 0 R] >>"
                .to_vec(),
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+PptxUnicodeCmapFace \
              /CIDSystemInfo << /Registry (Adobe) /Ordering (GB1) /Supplement 0 >> \
              /CIDToGIDMap /Identity /FontDescriptor 7 0 R /DW 600 >>"
                .to_vec(),
            b"<< /Type /FontDescriptor /FontName /ABCDEF+PptxUnicodeCmapFace /Flags 32 /FontFile2 8 0 R >>"
                .to_vec(),
            stream("", &partial_ttf),
        ]);

        // 2. `/UniGB-UTF16-H` with an explicit 4-byte UTF-16 surrogate
        //    `/ToUnicode` mapping `<D840DC00> -> <0041>` ('A'): the display
        //    list must keep the packed source bytes `0xD840DC00` + `code_len=4`
        //    so PPTX export resolves the explicit `/ToUnicode` entry ('A')
        //    instead of dropping it or emitting U+20000, while `<0042>` without
        //    `/ToUnicode` still decodes as 'B' through the CMap.
        let utf16_to_unicode = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
            1 begincodespacerange <0000> <FFFF> endcodespacerange \
            1 beginbfchar <D840DC00> <0041> endbfchar \
            endcmap CMapName currentdict /CMap defineresource pop end end";
        let utf16_pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] \
              /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
            stream("", b"0 0 0 rg BT /F1 18 Tf 20 50 Td <D840DC000042> Tj ET"),
            b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+PptxUnicodeCmapFace \
              /Encoding /UniGB-UTF16-H /DescendantFonts [6 0 R] /ToUnicode 9 0 R >>"
                .to_vec(),
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+PptxUnicodeCmapFace \
              /CIDSystemInfo << /Registry (Adobe) /Ordering (GB1) /Supplement 0 >> \
              /CIDToGIDMap /Identity /FontDescriptor 7 0 R /DW 600 >>"
                .to_vec(),
            b"<< /Type /FontDescriptor /FontName /ABCDEF+PptxUnicodeCmapFace /Flags 32 /FontFile2 8 0 R >>"
                .to_vec(),
            stream("", &partial_ttf),
            stream("", utf16_to_unicode),
        ]);

        for (label, pdf, expected_code, expected_len) in [
            ("unigb-ucs2-no-tounicode", ucs2_pdf, 0x0041u32, 2u8),
            (
                "unigb-utf16-surrogate-tounicode",
                utf16_pdf,
                0xD840_DC00u32,
                4u8,
            ),
        ] {
            let doc = zpdf_document::PdfDocument::open(pdf)
                .unwrap()
                .with_font_provider(Arc::clone(&provider));
            let page = doc.page(0).unwrap();
            let mut fonts = doc.load_page_fonts(&page);
            let (content, _) = doc.page_content_with_diagnostics(&page).unwrap();
            let mut images = ImageCache::new();
            let interpreter =
                zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
                    .with_document(doc.file(), &page.resources)
                    .with_fonts(&mut fonts)
                    .with_shared_fonts(&doc)
                    .with_images(&mut images);
            let (display_list, _) = interpreter.interpret_with_stats(&content);

            let (fid_primary, primary) = fonts.get_by_name("F1").unwrap();
            assert!(!primary.is_substitute, "{label}");
            let runs: Vec<_> = display_list
                .commands
                .iter()
                .filter_map(|cmd| match cmd {
                    RenderCommand::DrawGlyphRun(run) => Some(run),
                    _ => None,
                })
                .collect();
            assert_eq!(
                runs.len(),
                2,
                "{label}: recovered first glyph + embedded 'B'"
            );
            assert_ne!(runs[0].font_id, fid_primary, "{label}");
            assert_eq!(runs[0].glyphs[0].code, expected_code, "{label}");
            assert_eq!(runs[0].glyphs[0].code_len, expected_len, "{label}");

            let slide = display_list_to_slide(
                &display_list,
                0,
                &fonts,
                &images,
                &ConversionOptions::default(),
            )
            .expect("slide export");
            let slide_text: String = slide
                .elements
                .iter()
                .filter_map(|e| match e {
                    SlideElement::TextBox { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                slide_text, "AB",
                "{label}: PPTX slide export must decode recovered Type 0 text through logical font decoder"
            );
        }
    }

    #[test]
    fn mixed_width_tounicode_with_both_widths_exports_same_text_in_both_orders() {
        use std::sync::Arc;
        use zpdf_font::{CidWidths, FontProvider, LoadedFont, PdfFontType};

        const PRIMARY_TTF: &[u8] =
            include_bytes!("../../zpdf-font/tests/fixtures/coverage-primary.ttf");

        let probe = LoadedFont::new_with_data(
            PdfFontType::TrueType,
            "Probe".into(),
            PRIMARY_TTF.to_vec(),
            CidWidths::new(1000.0),
        );
        let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;
        let gid_b = probe.unicode_glyph('B' as u32).unwrap().0;
        assert_ne!(gid_a, gid_b);

        let stream = |dict: &str, bytes: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", bytes.len()).into_bytes();
            out.extend_from_slice(bytes);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (idx, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", idx + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            let count = objects.len() + 1;
            pdf.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                    .as_bytes(),
            );
            pdf
        };

        let mixed_cmap = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
            /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> def \
            /CMapName /PptxMixedWidth def /CMapType 1 def \
            2 begincodespacerange <20> <7E> <0000> <1FFF> endcodespacerange \
            2 begincidchar <41> 1 <0041> 2 endcidchar \
            endcmap CMapName currentdict /CMap defineresource pop end end";

        let provider = Arc::new(FontProvider::memory_only());
        provider
            .register_with_aliases(PRIMARY_TTF, 0, &["PptxMixedWidthFace"])
            .unwrap();

        for (label, to_unicode) in [
            (
                "short-then-long",
                b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
                  2 begincodespacerange <00> <FF> <0000> <FFFF> endcodespacerange \
                  2 beginbfchar <41> <0041> <0041> <0042> endbfchar \
                  endcmap CMapName currentdict /CMap defineresource pop end end"
                    .as_slice(),
            ),
            (
                "long-then-short",
                b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
                  2 begincodespacerange <00> <FF> <0000> <FFFF> endcodespacerange \
                  2 beginbfchar <0041> <0042> <41> <0041> endbfchar \
                  endcmap CMapName currentdict /CMap defineresource pop end end"
                    .as_slice(),
            ),
        ] {
            let pdf = assemble(&[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] \
                  /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                    .to_vec(),
                stream("", b"0 0 0 rg BT /F1 18 Tf 20 50 Td <410041> Tj ET"),
                b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+PptxMixedWidthFace \
                  /Encoding 9 0 R /DescendantFonts [6 0 R] /ToUnicode 10 0 R >>"
                    .to_vec(),
                b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+PptxMixedWidthFace \
                  /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
                  /CIDToGIDMap /Identity /FontDescriptor 7 0 R /DW 600 >>"
                    .to_vec(),
                b"<< /Type /FontDescriptor /FontName /ABCDEF+PptxMixedWidthFace /Flags 32 /FontFile2 8 0 R >>"
                    .to_vec(),
                stream("", b"corrupt unparseable truetype bytes"),
                stream("", mixed_cmap),
                stream("", to_unicode),
            ]);

            let doc = zpdf_document::PdfDocument::open(pdf)
                .unwrap()
                .with_font_provider(Arc::clone(&provider));
            let page = doc.page(0).unwrap();
            let mut fonts = doc.load_page_fonts(&page);
            let (content, _) = doc.page_content_with_diagnostics(&page).unwrap();
            let mut images = ImageCache::new();
            let mut extracted = Vec::new();
            let interpreter =
                zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
                    .with_document(doc.file(), &page.resources)
                    .with_fonts(&mut fonts)
                    .with_shared_fonts(&doc)
                    .with_images(&mut images)
                    .with_text_sink(&mut extracted);
            let (display_list, _) = interpreter.interpret_with_stats(&content);

            let extracted_text: String = extracted.iter().map(|s| s.text.as_str()).collect();
            assert_eq!(
                extracted_text, "AB",
                "{label}: text extraction must preserve both <41> and <0041> in one /ToUnicode"
            );

            let runs: Vec<_> = display_list
                .commands
                .iter()
                .filter_map(|cmd| match cmd {
                    RenderCommand::DrawGlyphRun(run) => Some(run),
                    _ => None,
                })
                .collect();
            assert_eq!(runs.len(), 1, "{label}");
            assert_eq!(
                [runs[0].glyphs[0].glyph_id, runs[0].glyphs[1].glyph_id],
                [gid_a, gid_b],
                "{label}: physical glyphs must follow <41>->A and <0041>->B regardless of order"
            );
            assert_eq!(
                [runs[0].glyphs[0].code_len, runs[0].glyphs[1].code_len],
                [1, 2],
                "{label}"
            );

            let slide = display_list_to_slide(
                &display_list,
                0,
                &fonts,
                &images,
                &ConversionOptions::default(),
            )
            .expect("slide export");
            let slide_text: String = slide
                .elements
                .iter()
                .filter_map(|e| match e {
                    SlideElement::TextBox { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                slide_text, "AB",
                "{label}: PPTX slide export must preserve both <41> and <0041> regardless of declaration order"
            );
        }
    }

    #[test]
    fn unicode_cmap_xml_forbidden_control_char_serializes_valid_slide_xml() {
        use quick_xml::events::Event;
        use std::sync::Arc;
        use zpdf_font::FontProvider;

        const PRIMARY_TTF: &[u8] =
            include_bytes!("../../zpdf-font/tests/fixtures/coverage-primary.ttf");

        let stream = |dict: &str, bytes: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", bytes.len()).into_bytes();
            out.extend_from_slice(bytes);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (idx, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", idx + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            let count = objects.len() + 1;
            pdf.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                    .as_bytes(),
            );
            pdf
        };

        let provider = Arc::new(FontProvider::memory_only());
        provider
            .register_with_aliases(PRIMARY_TTF, 0, &["PptxXmlControlFace"])
            .unwrap();

        // `/UniGB-UCS2-H` with NO `/ToUnicode` decodes `<0041000B0042>` to
        // `"A\u{000B}B"`. Serializing to PPTX must strip the XML-forbidden
        // vertical tab while keeping a well-formed slide whose `<a:t>` holds
        // both letters.
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] \
              /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
            stream("", b"0 0 0 rg BT /F1 18 Tf 20 50 Td <0041000B0042> Tj ET"),
            b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+PptxXmlControlFace \
              /Encoding /UniGB-UCS2-H /DescendantFonts [6 0 R] >>"
                .to_vec(),
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+PptxXmlControlFace \
              /CIDSystemInfo << /Registry (Adobe) /Ordering (GB1) /Supplement 0 >> \
              /CIDToGIDMap /Identity /FontDescriptor 7 0 R /DW 600 >>"
                .to_vec(),
            b"<< /Type /FontDescriptor /FontName /ABCDEF+PptxXmlControlFace /Flags 32 /FontFile2 8 0 R >>"
                .to_vec(),
            stream("", b"corrupt unparseable truetype bytes"),
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf)
            .unwrap()
            .with_font_provider(Arc::clone(&provider));
        let page = doc.page(0).unwrap();
        let mut fonts = doc.load_page_fonts(&page);
        let (content, _) = doc.page_content_with_diagnostics(&page).unwrap();
        let mut images = ImageCache::new();
        let interpreter = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_shared_fonts(&doc)
            .with_images(&mut images);
        let (display_list, _) = interpreter.interpret_with_stats(&content);

        let slide = display_list_to_slide(
            &display_list,
            0,
            &fonts,
            &images,
            &ConversionOptions::default(),
        )
        .expect("slide export");
        let raw_slide_text: String = slide
            .elements
            .iter()
            .filter_map(|e| match e {
                SlideElement::TextBox { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            raw_slide_text, "A\u{000B}B",
            "test fixture must exercise U+000B decoded via /UniGB-UCS2-H"
        );
        let presentation = PptxPresentation {
            slides: vec![slide],
            width_emu: pt_to_emu(200.0),
            height_emu: pt_to_emu(100.0),
        };
        let bytes = PptxWriter::new(presentation)
            .write_to_bytes()
            .expect("pptx bytes");
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip archive");
        let xml = String::from_utf8(zip_entry(&mut archive, "ppt/slides/slide1.xml"))
            .expect("utf-8 slide xml");

        // Enforce XML 1.0 §2.2 Char range across the entire serialized slide.
        for ch in xml.chars() {
            assert!(
                ooxml::is_valid_xml_char(ch),
                "serialized slide XML contains XML 1.0 forbidden character U+{:04X}: {xml}",
                ch as u32
            );
        }

        // Parse the serialized slide with quick-xml and collect `<a:t>` text.
        let mut reader = quick_xml::Reader::from_str(&xml);
        let mut in_text = false;
        let mut parsed_text = String::new();
        loop {
            match reader.read_event().expect("well-formed slide XML") {
                Event::Start(e) => {
                    for attr in e.attributes() {
                        attr.expect("valid attribute");
                    }
                    if e.name().as_ref() == "a:t" {
                        in_text = true;
                    }
                }
                Event::Empty(e) => {
                    for attr in e.attributes() {
                        attr.expect("valid attribute");
                    }
                }
                Event::End(e) => {
                    if e.name().as_ref() == "a:t" {
                        in_text = false;
                    }
                }
                Event::Text(e) => {
                    if in_text {
                        parsed_text.push_str(&e.xml10_content());
                    }
                }
                Event::GeneralRef(e) => {
                    if let Some(ch) = e.resolve_char_ref().expect("valid char ref") {
                        assert!(
                            ooxml::is_valid_xml_char(ch),
                            "serialized slide XML contains forbidden char ref U+{:04X}",
                            ch as u32
                        );
                        if in_text {
                            parsed_text.push(ch);
                        }
                    } else if in_text {
                        let entity = match e.xml10_content().as_ref() {
                            "amp" => "&",
                            "lt" => "<",
                            "gt" => ">",
                            "quot" => "\"",
                            "apos" => "'",
                            other => panic!("unexpected XML entity &{other};"),
                        };
                        parsed_text.push_str(entity);
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        assert_eq!(
            parsed_text, "AB",
            "parsed slide XML must retain both 'A' and 'B' without XML-forbidden U+000B"
        );
    }

    #[test]
    fn type3_word_with_to_unicode_exports_editable_text_box_without_charproc_shapes() {
        let stream = |dict: &str, bytes: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", bytes.len()).into_bytes();
            out.extend_from_slice(bytes);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (idx, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", idx + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            let count = objects.len() + 1;
            pdf.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                    .as_bytes(),
            );
            pdf
        };

        let to_unicode = b"/CIDInit /ProcSet findresource begin\n\
            12 dict begin begincmap\n\
            1 begincodespacerange <01> <03> endcodespacerange\n\
            3 beginbfchar\n\
            <01> <0043>\n\
            <02> <0061>\n\
            <03> <0074>\n\
            endbfchar\n\
            endcmap CMapName currentdict /CMap defineresource pop end end";
        let glyph_rect = b"1000 0 d0 0 0 800 800 re f";
        let content = b"0.9 0.9 0.9 rg 5 5 190 90 re f\n\
                        0 0 1 rg BT /F1 24 Tf 20 40 Td <010203> Tj ET\n\
                        1 0 0 rg 5 96 50 3 re f";

        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(),
            stream("", content),
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /ToUnicode 8 0 R /FirstChar 1 /LastChar 3 /Widths [600 500 400] \
              /Resources << >> >>"
                .to_vec(),
            b"<< /g1 9 0 R /g2 9 0 R /g3 9 0 R >>".to_vec(),
            b"<< /Type /Encoding /Differences [1 /g1 /g2 /g3] >>".to_vec(),
            stream("", to_unicode),
            stream("", glyph_rect),
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let interpreter = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images);
        let (display_list, _) = interpreter.interpret_with_stats(&content_bytes);

        // Visual commands contain only the expanded fills (no DrawGlyphRun so
        // renderers paint each CharProc once), while the semantic run is kept.
        assert!(
            !display_list
                .commands
                .iter()
                .any(|c| matches!(c, RenderCommand::DrawGlyphRun(_))),
            "Type 3 runs must not emit a duplicate DrawGlyphRun visual command"
        );
        assert_eq!(display_list.expanded_glyph_runs().len(), 1);

        let slide = display_list_to_slide(
            &display_list,
            0,
            &fonts,
            &images,
            &ConversionOptions::default(),
        )
        .expect("slide export");

        // Expect [background Rectangle, TextBox("Cat"), foreground Rectangle] —
        // no extra drawing objects from the 3 Type 3 CharProc fills.
        assert_eq!(
            slide.elements.len(),
            3,
            "unexpected slide elements: {:?}",
            slide.elements
        );
        assert!(matches!(slide.elements[0], SlideElement::Rectangle { .. }));
        match &slide.elements[1] {
            SlideElement::TextBox {
                text,
                color_rgb,
                font_size_pt,
                ..
            } => {
                assert_eq!(text, "Cat");
                assert_eq!(*color_rgb, (0, 0, 255));
                assert!((*font_size_pt - 24.0).abs() < 1e-6);
            }
            other => panic!("expected TextBox for Type 3 word, got {other:?}"),
        }
        assert!(matches!(slide.elements[2], SlideElement::Rectangle { .. }));

        let presentation = PptxPresentation {
            slides: vec![slide],
            width_emu: pt_to_emu(200.0),
            height_emu: pt_to_emu(100.0),
        };
        let bytes = PptxWriter::new(presentation)
            .write_to_bytes()
            .expect("pptx bytes");
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip archive");
        let xml = String::from_utf8(zip_entry(&mut archive, "ppt/slides/slide1.xml"))
            .expect("utf-8 slide xml");
        assert!(
            xml.contains("<a:t>Cat</a:t>"),
            "serialized slide XML must contain editable Type 3 text: {xml}"
        );
    }

    #[test]
    fn type3_unmapped_icon_without_text_falls_back_to_drawing_objects() {
        let stream = |dict: &str, bytes: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", bytes.len()).into_bytes();
            out.extend_from_slice(bytes);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (idx, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", idx + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            let count = objects.len() + 1;
            pdf.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                    .as_bytes(),
            );
            pdf
        };

        // Code 0x01 has custom glyph name /logo and no /ToUnicode, so it
        // decodes to empty text and should fall back to exporting its shape.
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(),
            stream("", b"1 0 0 rg BT /F1 40 Tf 10 10 Td <01> Tj ET"),
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 1 /LastChar 1 /Widths [1000] /Resources << >> >>"
                .to_vec(),
            b"<< /logo 8 0 R >>".to_vec(),
            b"<< /Type /Encoding /Differences [1 /logo] >>".to_vec(),
            stream("", b"1000 0 d0 0 0 1000 1000 re f"),
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let interpreter = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images);
        let (display_list, _) = interpreter.interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(
            &display_list,
            0,
            &fonts,
            &images,
            &ConversionOptions::default(),
        )
        .expect("slide export");

        assert_eq!(slide.elements.len(), 1);
        assert!(matches!(slide.elements[0], SlideElement::Rectangle { .. }));
    }

    #[test]
    fn type3_unmapped_logo_at_printable_ascii_code_exports_as_drawing_not_letter() {
        let stream = |dict: &str, bytes: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", bytes.len()).into_bytes();
            out.extend_from_slice(bytes);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (idx, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", idx + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            let count = objects.len() + 1;
            pdf.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                    .as_bytes(),
            );
            pdf
        };

        // Code 65 (0x41) maps to custom glyph /logo with NO /ToUnicode.
        // Printable-ASCII fallback must NOT turn it into an editable "A".
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(),
            stream("", b"1 0 0 rg BT /F1 40 Tf 10 10 Td (A) Tj ET"),
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 65 /Widths [1000] /Resources << >> >>"
                .to_vec(),
            b"<< /logo 8 0 R >>".to_vec(),
            b"<< /Type /Encoding /Differences [65 /logo] >>".to_vec(),
            stream("", b"1000 0 d0 0 0 1000 1000 re f"),
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let interpreter = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images);
        let (display_list, _) = interpreter.interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(
            &display_list,
            0,
            &fonts,
            &images,
            &ConversionOptions::default(),
        )
        .expect("slide export");

        assert_eq!(slide.elements.len(), 1);
        assert!(
            matches!(slide.elements[0], SlideElement::Rectangle { .. }),
            "unmapped /logo at code 65 must export as drawing artwork, got {:?}",
            slide.elements
        );
    }

    #[test]
    fn type3_mixed_mapped_and_unmapped_run_preserves_text_and_icon_in_order() {
        let stream = |dict: &str, bytes: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", bytes.len()).into_bytes();
            out.extend_from_slice(bytes);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (idx, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", idx + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            let count = objects.len() + 1;
            pdf.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                    .as_bytes(),
            );
            pdf
        };

        // 0x41 -> /A (mapped), 0x42 -> /logo (unmapped custom glyph), 0x43 -> /C (mapped).
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(),
            stream("", b"0 0 0 rg BT /F1 30 Tf 10 30 Td (ABC) Tj ET"),
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 67 /Widths [1000 1000 1000] /Resources << >> >>"
                .to_vec(),
            b"<< /A 8 0 R /logo 8 0 R /C 8 0 R >>".to_vec(),
            b"<< /Type /Encoding /Differences [65 /A /logo /C] >>".to_vec(),
            stream("", b"1000 0 d0 0 0 800 800 re f"),
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let interpreter = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images);
        let (display_list, _) = interpreter.interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(
            &display_list,
            0,
            &fonts,
            &images,
            &ConversionOptions::default(),
        )
        .expect("slide export");

        assert_eq!(
            slide.elements.len(),
            3,
            "expected [TextBox(\"A\"), Rectangle(logo), TextBox(\"C\")], got {:?}",
            slide.elements
        );
        assert!(matches!(&slide.elements[0], SlideElement::TextBox { text, .. } if text == "A"));
        assert!(matches!(&slide.elements[1], SlideElement::Rectangle { .. }));
        assert!(matches!(&slide.elements[2], SlideElement::TextBox { text, .. } if text == "C"));
    }

    #[test]
    fn type3_oc_hidden_glyph_next_to_visible_letter_omitted_from_slide_text() {
        let stream = |dict: &str, bytes: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", bytes.len()).into_bytes();
            out.extend_from_slice(bytes);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (idx, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", idx + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            let count = objects.len() + 1;
            pdf.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                    .as_bytes(),
            );
            pdf
        };

        // /A and /D draw visible rectangles; /B installs a glyph-local clip and
        // puts its only fill inside a disabled BDC /OC group; /C invokes a Form
        // XObject carrying its own disabled /OC entry.
        let glyph_visible = stream("", b"1000 0 d0 0 0 800 800 re f");
        let glyph_clipped_hidden = stream(
            "",
            b"1000 0 d0 0 0 800 800 re W n /OC /LayerOff BDC 0 0 800 800 re f EMC",
        );
        let glyph_xobj_oc_hidden = stream("", b"1000 0 d0 /FmHidden Do");
        let form_oc_hidden = stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 1000 1000] /OC 11 0 R",
            b"0 0 800 800 re f",
        );
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R \
              /OCProperties << /OCGs [11 0 R] /D << /BaseState /ON /OFF [11 0 R] >> >> >>"
                .to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 100] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> /Properties << /LayerOff 11 0 R >> \
                            /XObject << /FmHidden 12 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"0 0 0 rg BT /F1 30 Tf 10 30 Td (ABCD) Tj ET"), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 68 /Widths [1000 1000 1000 1000] /Resources << >> >>"
                .to_vec(), // 5
            b"<< /A 8 0 R /B 9 0 R /C 10 0 R /D 8 0 R >>".to_vec(), // 6
            b"<< /Type /Encoding /Differences [65 /A /B /C /D] >>".to_vec(), // 7
            glyph_visible,                                         // 8
            glyph_clipped_hidden,                                  // 9
            glyph_xobj_oc_hidden,                                  // 10
            b"<< /Type /OCG /Name (Hidden) >>".to_vec(),           // 11
            form_oc_hidden,                                        // 12
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let oc = doc.oc_config().expect("oc config");
        let interpreter = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .with_optional_content(&oc);
        let (display_list, _) = interpreter.interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(
            &display_list,
            0,
            &fonts,
            &images,
            &ConversionOptions::default(),
        )
        .expect("slide export");

        let texts: Vec<&str> = slide
            .elements
            .iter()
            .filter_map(|el| match el {
                SlideElement::TextBox { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            vec!["A", "D"],
            "hidden glyphs 'B' (clipped BDC /OC) and 'C' (XObject /OC) must not appear in slide text; got {:?}",
            slide.elements
        );
        assert_eq!(
            slide.elements.len(),
            2,
            "no extra drawing shapes should be emitted for visible or hidden letters"
        );
    }

    #[test]
    fn type3_unmapped_patterned_stencil_logo_preserves_transparent_hole() {
        let stream = |dict: &str, bytes: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", bytes.len()).into_bytes();
            out.extend_from_slice(bytes);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (idx, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", idx + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            let count = objects.len() + 1;
            pdf.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                    .as_bytes(),
            );
            pdf
        };

        // Colored tiling pattern: red left half [0..45], blue right half [45..90].
        let pattern = stream(
            "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
             /BBox [0 0 90 90] /XStep 90 /YStep 90 /Resources << >>",
            b"1 0 0 rg 0 0 45 90 re f\n0 0 1 rg 45 0 45 90 re f",
        );
        // Unmapped /logo glyph: 4x4 ring /ImageMask true stencil (bit 0 = painted,
        // bit 1 = transparent 2x2 center hole across rows 1..3, cols 1..3).
        let mut glyph_bytes = b"1000 0 0 0 1000 1000 d1\n\
            1000 0 0 1000 0 0 cm\n\
            BI /IM true /W 4 /H 4 /BPC 1 ID "
            .to_vec();
        glyph_bytes.extend_from_slice(&[0x00, 0x60, 0x60, 0x00]);
        glyph_bytes.extend_from_slice(b"\nEI");

        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 90 90] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> /Pattern << /P1 9 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"/Pattern cs /P1 scn BT /F1 90 Tf 0 0 Td (A) Tj ET"), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 65 /Widths [1000] /Resources << >> >>"
                .to_vec(), // 5
            b"<< /logo 8 0 R >>".to_vec(),                 // 6
            b"<< /Type /Encoding /Differences [65 /logo] >>".to_vec(), // 7
            stream("", &glyph_bytes),                      // 8
            pattern,                                       // 9
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let interpreter = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images);
        let (display_list, _) = interpreter.interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(
            &display_list,
            0,
            &fonts,
            &images,
            &ConversionOptions::default(),
        )
        .expect("slide export");

        assert_eq!(
            slide.elements.len(),
            1,
            "patterned stencil logo must flatten to one transparent SlideElement::Image, got {:?}",
            slide.elements
        );
        let SlideElement::Image { image_data, .. } = &slide.elements[0] else {
            panic!("expected SlideElement::Image, got {:?}", slide.elements[0]);
        };
        let rgba = image::load_from_memory(image_data)
            .expect("decode exported PNG")
            .to_rgba8();
        let (w, h) = rgba.dimensions();

        // Center pixel lies inside the transparent hole of the 3x3 ring stencil.
        let center = rgba.get_pixel(w / 2, h / 2).0;
        assert_eq!(
            center[3], 0,
            "transparent hole in patterned Type 3 logo must survive PPTX export, got {center:?}"
        );
        // Outer left and right ring edges must be opaque with red and blue stripes.
        let left_ring = rgba.get_pixel(w / 10, h / 2).0;
        assert!(
            left_ring[3] > 200 && left_ring[0] > 200 && left_ring[2] < 40,
            "left ring pixel must preserve opaque red pattern, got {left_ring:?}"
        );
        let right_ring = rgba.get_pixel(w - w / 10 - 1, h / 2).0;
        assert!(
            right_ring[3] > 200 && right_ring[2] > 200 && right_ring[0] < 40,
            "right ring pixel must preserve opaque blue pattern, got {right_ring:?}"
        );
    }

    #[test]
    fn synthetic_partially_mapped_expanded_run_falls_back_to_drawings() {
        let mut char_procs = std::collections::HashMap::new();
        char_procs.insert(
            "A".to_string(),
            std::sync::Arc::<[u8]>::from(b"1000 0 d0 0 0 10 10 re f".as_slice()),
        );
        char_procs.insert(
            "logo".to_string(),
            std::sync::Arc::<[u8]>::from(b"1000 0 d0 0 0 10 10 re f".as_slice()),
        );
        let mut encoding = vec![String::new(); 67];
        encoding[65] = "A".to_string();
        encoding[66] = "logo".to_string();
        let font = LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [0.001, 0.0, 0.0, 0.001, 0.0, 0.0],
                char_procs,
                encoding,
                widths: vec![1000.0, 1000.0],
                first_char: 65,
            },
            "MixedType3".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(1000.0),
        );
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        let mut rect = zpdf_display_list::Path::new();
        rect.rect(Rect::new(10.0, 10.0, 20.0, 20.0));
        dl.push(RenderCommand::FillPath {
            path: rect.clone(),
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::FillPath {
            path: rect,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        dl.push_expanded_glyph_run(
            0,
            2,
            GlyphRun {
                font_id,
                font_size: 12.0,
                glyphs: vec![
                    PositionedGlyph {
                        glyph_id: 65,
                        code_len: 1,
                        code: 65,
                        x: 0.0,
                        y: 0.0,
                        advance: 12.0,
                    },
                    PositionedGlyph {
                        glyph_id: 66,
                        code_len: 1,
                        code: 66,
                        x: 12.0,
                        y: 0.0,
                        advance: 12.0,
                    },
                ],
                paint: Paint::Solid(Color::black()),
                alpha: 1.0,
                overprint: None,
                transform: Matrix::identity(),
                h_scale: 1.0,
            },
        );

        let slide = display_list_to_slide(
            &dl,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");

        assert_eq!(
            slide.elements.len(),
            2,
            "partially mapped synthetic expanded run must fall back to drawings: {:?}",
            slide.elements
        );
        assert!(slide
            .elements
            .iter()
            .all(|e| matches!(e, SlideElement::Rectangle { .. })));
    }

    #[test]
    fn type3_unmapped_stencil_with_pattern_lettering_masks_text_across_hole() {
        const BOLD_TTF: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/coverage-bold.ttf");

        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Colored tiling pattern whose cell draws solid red lettering across
        // the whole 90x90 cell (including straight across the stencil's center hole).
        let pattern = stream(
            "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
             /BBox [0 0 90 90] /XStep 90 /YStep 90 \
             /Resources << /Font << /F2 10 0 R >> >>",
            b"1 0 0 rg BT /F2 120 Tf 0 0 Td (AA) Tj ET",
        );
        // Unmapped /logo glyph: 4x4 ring /ImageMask true stencil with a 2x2
        // transparent center hole.
        let mut glyph_bytes = b"1000 0 0 0 1000 1000 d1\n\
            1000 0 0 1000 0 0 cm\n\
            BI /IM true /W 4 /H 4 /BPC 1 ID "
            .to_vec();
        glyph_bytes.extend_from_slice(&[0x00, 0x60, 0x60, 0x00]);
        glyph_bytes.extend_from_slice(b"\nEI");

        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 90 90] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> /Pattern << /P1 9 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"/Pattern cs /P1 scn BT /F1 90 Tf 0 0 Td (A) Tj ET"), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 65 /Widths [1000] /Resources << >> >>"
                .to_vec(), // 5
            b"<< /logo 8 0 R >>".to_vec(),                 // 6
            b"<< /Type /Encoding /Differences [65 /logo] >>".to_vec(), // 7
            stream("", &glyph_bytes),                      // 8
            pattern,                                       // 9
            b"<< /Type /Font /Subtype /TrueType /BaseFont /BoldSquare \
              /Encoding /WinAnsiEncoding /FirstChar 65 /LastChar 65 /Widths [600] \
              /FontDescriptor 11 0 R >>"
                .to_vec(), // 10
            b"<< /Type /FontDescriptor /FontName /BoldSquare /Flags 32 \
              /FontBBox [0 0 1000 1000] /ItalicAngle 0 /Ascent 800 /Descent -200 \
              /CapHeight 700 /StemV 80 /FontFile2 12 0 R >>"
                .to_vec(), // 11
            stream(&format!("/Length1 {}", BOLD_TTF.len()), BOLD_TTF), // 12
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let interpreter = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images);
        let (display_list, _) = interpreter.interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(
            &display_list,
            0,
            &fonts,
            &images,
            &ConversionOptions::default(),
        )
        .expect("slide export");

        assert_eq!(
            slide.elements.len(),
            1,
            "stencil painted by a tiling pattern with lettering must flatten to one Image and no free-standing TextBox, got {:?}",
            slide.elements
        );
        let SlideElement::Image { image_data, .. } = &slide.elements[0] else {
            panic!("expected SlideElement::Image, got {:?}", slide.elements[0]);
        };
        let rgba = image::load_from_memory(image_data)
            .expect("decode exported PNG")
            .to_rgba8();
        let (w, h) = rgba.dimensions();

        let center = rgba.get_pixel(w / 2, h / 2).0;
        assert_eq!(
            center[3], 0,
            "pattern lettering crossing the stencil hole must stay masked out at center, got {center:?}"
        );
        assert!(
            rgba.pixels()
                .any(|p| p.0[3] > 200 && p.0[0] > 200 && p.0[2] < 40),
            "pattern lettering must remain visible in the opaque stencil ring"
        );
    }

    #[test]
    fn reused_offset_soft_mask_survives_cropped_pptx_flattening() {
        let page_rect = Rect::new(0.0, 0.0, 600.0, 600.0);
        let mut mask_dl = DisplayList::new(page_rect);
        let mut mask_rect = zpdf_display_list::Path::new();
        mask_rect.rect(Rect::new(500.0, 500.0, 540.0, 540.0));
        mask_dl.push(RenderCommand::FillPath {
            path: mask_rect,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let mask = zpdf_display_list::SoftMask {
            kind: zpdf_display_list::SoftMaskKind::Alpha,
            commands: std::sync::Arc::new(mask_dl),
            offset: (-480.0, -480.0),
            backdrop_luma: 0.0,
            transfer: None,
        };

        let mut dl = DisplayList::new(page_rect);
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: page_rect,
            alpha: 1.0,
            mask: Some(mask),
        });
        let mut draw_rect = zpdf_display_list::Path::new();
        draw_rect.rect(Rect::new(20.0, 20.0, 60.0, 60.0));
        dl.push(RenderCommand::FillPath {
            path: draw_rect,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);

        let slide = display_list_to_slide(
            &dl,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");

        assert_eq!(
            slide.elements.len(),
            1,
            "reused offset mask tile must not disappear"
        );
        let SlideElement::Image { image_data, .. } = &slide.elements[0] else {
            panic!("expected SlideElement::Image, got {:?}", slide.elements[0]);
        };
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        let (w, h) = rgba.dimensions();
        assert!(
            w <= 100 && h <= 100,
            "tile should stay tightly cropped, got {w}x{h}"
        );
        let center = rgba.get_pixel(w / 2, h / 2).0;
        assert!(
            center[3] > 200 && center[2] > 200 && center[0] < 40,
            "tile center must render opaque blue through rebased offset mask, got {center:?}"
        );
    }

    #[test]
    fn alpha_mask_with_rectangle_and_text_preserves_text_revealed_area() {
        const BOLD_TTF: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/coverage-bold.ttf");
        let font = LoadedFont::new_with_data(
            zpdf_font::PdfFontType::TrueType,
            "BoldSquare".into(),
            BOLD_TTF.to_vec(),
            zpdf_font::CidWidths::new(600.0),
        );
        let gid_a = font.unicode_glyph('A' as u32).expect("gid for A").0;
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        let page_rect = Rect::new(0.0, 0.0, 300.0, 200.0);
        let mut mask_dl = DisplayList::new(page_rect);
        let mut small_rect = zpdf_display_list::Path::new();
        small_rect.rect(Rect::new(10.0, 10.0, 20.0, 20.0));
        mask_dl.push(RenderCommand::FillPath {
            path: small_rect,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        mask_dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 60.0,
            glyphs: vec![PositionedGlyph {
                glyph_id: gid_a,
                code_len: 1,
                code: b'A' as u32,
                x: 0.0,
                y: 0.0,
                advance: 36.0,
            }],
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
            transform: Matrix::translate(180.0, 100.0),
            h_scale: 1.0,
        }));
        let mask = zpdf_display_list::SoftMask {
            kind: zpdf_display_list::SoftMaskKind::Alpha,
            commands: std::sync::Arc::new(mask_dl),
            offset: (0.0, 0.0),
            backdrop_luma: 0.0,
            transfer: None,
        };

        let mut dl = DisplayList::new(page_rect);
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: page_rect,
            alpha: 1.0,
            mask: Some(mask),
        });
        let mut bg = zpdf_display_list::Path::new();
        bg.rect(page_rect);
        dl.push(RenderCommand::FillPath {
            path: bg,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 1.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);

        let slide = display_list_to_slide(
            &dl,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");

        assert_eq!(slide.elements.len(), 1);
        let SlideElement::Image {
            width_emu,
            image_data,
            ..
        } = &slide.elements[0]
        else {
            panic!("expected SlideElement::Image, got {:?}", slide.elements[0]);
        };
        assert!(
            *width_emu >= pt_to_emu(180.0),
            "mask crop must extend across both the small rect and distant text, got width_emu={width_emu}"
        );
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        let (w, h) = rgba.dimensions();
        let has_right_text_green = (w * 2 / 3..w).any(|x| {
            (0..h).any(|y| {
                let p = rgba.get_pixel(x, y).0;
                p[3] > 200 && p[1] > 200 && p[0] < 40
            })
        });
        assert!(
            has_right_text_green,
            "area revealed by text inside the alpha mask must survive export"
        );
    }

    #[test]
    fn large_page_masked_artwork_respects_raster_cap_and_physical_size() {
        let page_rect = Rect::new(0.0, 0.0, 16_384.0, 16_384.0);
        let mut mask_dl = DisplayList::new(page_rect);
        let mut full = zpdf_display_list::Path::new();
        full.rect(page_rect);
        mask_dl.push(RenderCommand::FillPath {
            path: full.clone(),
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let mask = zpdf_display_list::SoftMask {
            kind: zpdf_display_list::SoftMaskKind::Alpha,
            commands: std::sync::Arc::new(mask_dl),
            offset: (0.0, 0.0),
            backdrop_luma: 0.0,
            transfer: None,
        };

        let mut dl = DisplayList::new(page_rect);
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: page_rect,
            alpha: 1.0,
            mask: Some(mask),
        });
        dl.push(RenderCommand::FillPath {
            path: full,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);

        let slide = display_list_to_slide(
            &dl,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");

        assert_eq!(slide.elements.len(), 1);
        let SlideElement::Image {
            width_emu,
            height_emu,
            image_data,
            ..
        } = &slide.elements[0]
        else {
            panic!("expected SlideElement::Image, got {:?}", slide.elements[0]);
        };
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        let (w, h) = rgba.dimensions();
        assert!(
            w <= 2048 && h <= 2048,
            "large-page masked raster must stay within 2048px per side, got {w}x{h}"
        );
        assert!(
            *width_emu >= pt_to_emu(16_383.0) && *height_emu >= pt_to_emu(16_383.0),
            "physical slide dimensions must preserve full 16384pt crop, got {width_emu}x{height_emu}"
        );
    }

    #[test]
    fn small_rectangle_under_page_wide_alpha_mask_produces_small_image() {
        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        let mut mask_dl = DisplayList::new(page_rect);
        let mut full = zpdf_display_list::Path::new();
        full.rect(page_rect);
        mask_dl.push(RenderCommand::FillPath {
            path: full,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let mask = zpdf_display_list::SoftMask {
            kind: zpdf_display_list::SoftMaskKind::Alpha,
            commands: std::sync::Arc::new(mask_dl),
            offset: (0.0, 0.0),
            backdrop_luma: 0.0,
            transfer: None,
        };

        let mut dl = DisplayList::new(page_rect);
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: page_rect,
            alpha: 1.0,
            mask: Some(mask),
        });
        let mut small = zpdf_display_list::Path::new();
        small.rect(Rect::new(100.0, 200.0, 120.0, 220.0));
        dl.push(RenderCommand::FillPath {
            path: small,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);

        let slide = display_list_to_slide(
            &dl,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");

        assert_eq!(slide.elements.len(), 1);
        let SlideElement::Image {
            width_emu,
            height_emu,
            image_data,
            ..
        } = &slide.elements[0]
        else {
            panic!("expected SlideElement::Image, got {:?}", slide.elements[0]);
        };
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        let (w, h) = rgba.dimensions();
        assert!(
            w <= 64 && h <= 64,
            "small rectangle under page-wide alpha mask must produce a small raster, got {w}x{h}"
        );
        assert!(
            *width_emu <= pt_to_emu(25.0) && *height_emu <= pt_to_emu(25.0),
            "physical slide dimensions must match the small rectangle, got {width_emu}x{height_emu}"
        );
        let center = rgba.get_pixel(w / 2, h / 2).0;
        assert!(
            center[3] > 200 && center[2] > 200 && center[0] < 40,
            "small masked rectangle center must be opaque blue, got {center:?}"
        );
    }

    #[test]
    fn type3_mapped_d0_glyph_with_local_color_preserves_drawing_in_pptx() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // /A is a mapped d0 glyph that paints red (differing from caller's black fill).
        // /B is a mapped d0 glyph that paints black (matching caller's black fill).
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"0 0 0 rg BT /F1 40 Tf 10 30 Td (AB) Tj ET"), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 66 /Widths [1000 1000] /Resources << >> >>"
                .to_vec(), // 5
            b"<< /A 8 0 R /B 9 0 R >>".to_vec(),           // 6
            b"<< /Type /Encoding /Differences [65 /A /B] >>".to_vec(), // 7
            stream("", b"1000 0 d0 1 0 0 rg 0 0 800 800 re f"), // 8 (red d0)
            stream("", b"1000 0 d0 0 0 0 rg 0 0 800 800 re f"), // 9 (black d0)
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let interpreter = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images);
        let (display_list, _) = interpreter.interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(
            &display_list,
            0,
            &fonts,
            &images,
            &ConversionOptions::default(),
        )
        .expect("slide export");

        assert!(
            slide.elements.iter().any(|e| matches!(
                e,
                SlideElement::Rectangle {
                    fill_rgb: Some((255, 0, 0)),
                    ..
                }
            )),
            "mapped red d0 glyph under black caller fill must keep its red drawing, got {:?}",
            slide.elements
        );
        let texts: Vec<&str> = slide
            .elements
            .iter()
            .filter_map(|e| match e {
                SlideElement::TextBox { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            vec!["B"],
            "caller-colored mapped d0 glyph 'B' must stay editable while red 'A' keeps its drawing"
        );
    }

    #[test]
    fn inline_image_opacity_preserved_in_exported_png() {
        let make_slide = |alpha: f32| {
            let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 40.0, 40.0));
            dl.push(RenderCommand::DrawInlineImage(
                zpdf_display_list::InlineImageDraw {
                    image: std::sync::Arc::new(zpdf_display_list::InlineImage {
                        width: 2,
                        height: 2,
                        data: [255, 0, 0, 255].repeat(4),
                        has_alpha: false,
                        is_image_mask: false,
                        premultiplied: false,
                    }),
                    transform: Matrix::new(20.0, 0.0, 0.0, 20.0, 10.0, 10.0),
                    alpha,
                },
            ));
            let slide = display_list_to_slide(
                &dl,
                0,
                &FontCache::new(),
                &ImageCache::new(),
                &ConversionOptions::default(),
            )
            .expect("slide export");
            let SlideElement::Image { image_data, .. } = &slide.elements[0] else {
                panic!("expected SlideElement::Image");
            };
            image::load_from_memory(image_data)
                .expect("decode PNG")
                .to_rgba8()
                .get_pixel(0, 0)
                .0
        };

        assert_eq!(
            make_slide(0.25),
            [255, 0, 0, 64],
            "alpha=0.25 must modulate exported PNG alpha while preserving RGB"
        );
        assert_eq!(
            make_slide(1.0),
            [255, 0, 0, 255],
            "alpha=1.0 must keep opaque pixels unchanged"
        );
    }

    #[test]
    fn type3_mapped_d0_glyph_with_hiding_soft_mask_stays_hidden_in_pptx() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // /A is a mapped d0 glyph that installs a fully transparent alpha soft mask
        // and fills a path in the caller's black color.
        // /B is an unmasked black d0 glyph.
        // /C is a mapped d0 glyph that installs the same transparent alpha soft mask
        // and draws a letter using an ordinary nested TrueType font.
        const BOLD_TTF: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/coverage-bold.ttf");
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 100] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"0 0 0 rg BT /F1 40 Tf 10 30 Td (ABC) Tj ET"), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 67 /Widths [1000 1000 1000] \
              /Resources << /ExtGState << /GSHide 10 0 R >> /Font << /FNest 13 0 R >> >> >>"
                .to_vec(), // 5
            b"<< /A 8 0 R /B 9 0 R /C 12 0 R >>".to_vec(), // 6
            b"<< /Type /Encoding /Differences [65 /A /B /C] >>".to_vec(), // 7
            stream("", b"1000 0 d0 /GSHide gs 0 0 0 rg 0 0 800 800 re f"), // 8 (masked path)
            stream("", b"1000 0 d0 0 0 0 rg 0 0 800 800 re f"), // 9 (visible B)
            b"<< /Type /ExtGState /SMask << /Type /Mask /S /Alpha /G 11 0 R >> >>".to_vec(), // 10
            stream(
                "/Type /XObject /Subtype /Form /BBox [0 0 1000 1000] \
                 /Group << /Type /Group /S /Transparency /CS /DeviceRGB >>",
                b"",
            ), // 11 (empty alpha mask -> 0 coverage)
            stream(
                "",
                b"1000 0 d0 /GSHide gs 0 0 0 rg BT /FNest 800 Tf 0 0 Td (A) Tj ET",
            ), // 12 (masked nested ordinary font)
            b"<< /Type /Font /Subtype /TrueType /BaseFont /BoldSquare \
              /FirstChar 65 /LastChar 65 /Widths [600] /Encoding /WinAnsiEncoding \
              /FontDescriptor 14 0 R >>"
                .to_vec(), // 13
            b"<< /Type /FontDescriptor /FontName /BoldSquare /Flags 32 \
              /FontBBox [0 0 1000 1000] /ItalicAngle 0 /Ascent 800 /Descent -200 \
              /CapHeight 700 /StemV 80 /FontFile2 15 0 R >>"
                .to_vec(), // 14
            stream(&format!("/Length1 {}", BOLD_TTF.len()), BOLD_TTF), // 15
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let interpreter = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images);
        let (display_list, _) = interpreter.interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(
            &display_list,
            0,
            &fonts,
            &images,
            &ConversionOptions::default(),
        )
        .expect("slide export");

        assert_eq!(
            slide.elements.len(),
            1,
            "fully masked /A (path) and /C (nested font) must emit neither text boxes nor visible drawings, leaving only /B: {:?}",
            slide.elements
        );
        match &slide.elements[0] {
            SlideElement::TextBox { text, .. } => assert_eq!(text, "B"),
            other => panic!("expected single TextBox(\"B\"), got {other:?}"),
        }
    }

    #[test]
    fn type3_metrics_only_letter_stays_blank_in_pptx_while_preserving_advances_and_extraction() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // /A only declares its width with d0 (zero paint); /B and /C paint in caller fill.
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 100] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"0 0 0 rg BT /F1 40 Tf 10 30 Td (BAC) Tj ET"), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 67 /Widths [1000 1000 1000] /Resources << >> >>"
                .to_vec(), // 5
            b"<< /A 8 0 R /B 9 0 R /C 10 0 R >>".to_vec(), // 6
            b"<< /Type /Encoding /Differences [65 /A /B /C] >>".to_vec(), // 7
            stream("", b"1000 0 d0"),                      // 8 (metrics-only /A)
            stream("", b"1000 0 d0 0 0 800 800 re f"),     // 9 (painted /B)
            stream("", b"1000 0 d0 0 0 800 800 re f"),     // 10 (painted /C)
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let mut spans = Vec::new();
        let interpreter = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .with_text_sink(&mut spans);
        let (display_list, _) = interpreter.interpret_with_stats(&content_bytes);

        let extracted: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(
            extracted, "BAC",
            "text extraction must retain the Unicode mapping of metrics-only /A"
        );

        let slide = display_list_to_slide(
            &display_list,
            0,
            &fonts,
            &images,
            &ConversionOptions::default(),
        )
        .expect("slide export");

        let boxes: Vec<(&str, i64, i64)> = slide
            .elements
            .iter()
            .filter_map(|e| match e {
                SlideElement::TextBox {
                    text,
                    x_emu,
                    width_emu,
                    ..
                } => Some((text.as_str(), *x_emu, *width_emu)),
                _ => None,
            })
            .collect();
        assert_eq!(
            boxes,
            vec![
                ("B", pt_to_emu(10.0), pt_to_emu(40.0)),
                ("C", pt_to_emu(90.0), pt_to_emu(40.0)),
            ],
            "metrics-only /A must stay blank in PPTX while preserving /B and /C advances"
        );
    }

    #[test]
    fn rebase_soft_masks_preserves_sharing_across_repeated_nested_masks() {
        let page_rect = Rect::new(0.0, 0.0, 200.0, 200.0);

        // Leaf mask with an offset (-40, -40) so rebasing must translate it.
        let mut leaf_dl = DisplayList::new(page_rect);
        let mut leaf_rect = zpdf_display_list::Path::new();
        leaf_rect.rect(Rect::new(60.0, 60.0, 100.0, 100.0));
        leaf_dl.push(RenderCommand::FillPath {
            path: leaf_rect,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let mut child_arc = std::sync::Arc::new(leaf_dl);
        let mut child_offset = (-40.0f32, -40.0f32);

        // Build a 20-level shared DAG where each level references the child mask twice
        // (2^20 = 1,048,576 paths without memoization).
        for _ in 0..20 {
            let mut parent_dl = DisplayList::new(page_rect);
            for _ in 0..2 {
                parent_dl.push(RenderCommand::PushBlendGroup {
                    blend_mode: zpdf_display_list::BlendMode::Normal,
                    isolated: true,
                    knockout: false,
                    bounds: page_rect,
                    alpha: 1.0,
                    mask: Some(zpdf_display_list::SoftMask {
                        kind: zpdf_display_list::SoftMaskKind::Alpha,
                        commands: std::sync::Arc::clone(&child_arc),
                        offset: child_offset,
                        backdrop_luma: 0.0,
                        transfer: None,
                    }),
                });
                parent_dl.push(RenderCommand::PopBlendGroup);
            }
            child_arc = std::sync::Arc::new(parent_dl);
            child_offset = (0.0, 0.0);
        }

        let top_mask = zpdf_display_list::SoftMask {
            kind: zpdf_display_list::SoftMaskKind::Alpha,
            commands: std::sync::Arc::clone(&child_arc),
            offset: (0.0, 0.0),
            backdrop_luma: 0.0,
            transfer: None,
        };
        let top_cmds = vec![
            RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: page_rect,
                alpha: 1.0,
                mask: Some(top_mask.clone()),
            },
            RenderCommand::PopBlendGroup,
            RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: page_rect,
                alpha: 1.0,
                mask: Some(top_mask),
            },
            RenderCommand::PopBlendGroup,
        ];

        let rebased = rebase_soft_masks(&top_cmds);
        let RenderCommand::PushBlendGroup { mask: Some(m0), .. } = &rebased[0] else {
            panic!("expected first rebased mask");
        };
        let RenderCommand::PushBlendGroup { mask: Some(m1), .. } = &rebased[2] else {
            panic!("expected second rebased mask");
        };
        assert!(
            std::sync::Arc::ptr_eq(&m0.commands, &m1.commands),
            "repeated top-level references must share one rebased Arc<DisplayList>"
        );

        // Walk all 20 levels and verify sibling references still share one Arc<DisplayList>.
        let mut cur = std::sync::Arc::clone(&m0.commands);
        for level in 0..20 {
            let RenderCommand::PushBlendGroup { mask: Some(a), .. } = &cur.commands[0] else {
                panic!("missing first child mask at level {level}");
            };
            let RenderCommand::PushBlendGroup { mask: Some(b), .. } = &cur.commands[2] else {
                panic!("missing second child mask at level {level}");
            };
            assert_eq!(a.offset, (0.0, 0.0));
            assert!(
                std::sync::Arc::ptr_eq(&a.commands, &b.commands),
                "sibling nested mask references at level {level} must share one translated Arc"
            );
            cur = std::sync::Arc::clone(&a.commands);
        }
        // Verify the leaf rectangle was shifted from [60..100] by (-40, -40) -> [20..60].
        let RenderCommand::FillPath { path, .. } = &cur.commands[0] else {
            panic!("expected leaf FillPath");
        };
        let xs: Vec<f64> = path.points().iter().map(|p| p.x).collect();
        assert!(
            xs.iter().all(|&x| (20.0..=60.0).contains(&x)),
            "leaf mask must be rebased to [20..60], got {xs:?}"
        );
    }

    #[test]
    fn pattern_range_expanded_glyph_run_is_not_converted_and_does_not_veto_mask() {
        let mut char_procs = std::collections::HashMap::new();
        char_procs.insert(
            "A".to_string(),
            std::sync::Arc::from(b"1000 0 d0 0 0 800 800 re f".as_slice()),
        );
        let mut encoding = vec![String::new(); 256];
        encoding[65] = "A".to_string();
        let t3 = LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [0.001, 0.0, 0.0, 0.001, 0.0, 0.0],
                char_procs,
                encoding,
                widths: vec![1000.0],
                first_char: 65,
            },
            "T3Pat".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(1000.0),
        );
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), t3);

        let page_rect = Rect::new(0.0, 0.0, 100.0, 100.0);
        // Alpha mask reveals only the left half [0..50].
        let mut mask_dl = DisplayList::new(page_rect);
        let mut left = zpdf_display_list::Path::new();
        left.rect(Rect::new(0.0, 0.0, 50.0, 100.0));
        mask_dl.push(RenderCommand::FillPath {
            path: left,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let mask = zpdf_display_list::SoftMask {
            kind: zpdf_display_list::SoftMaskKind::Alpha,
            commands: std::sync::Arc::new(mask_dl),
            offset: (0.0, 0.0),
            backdrop_luma: 0.0,
            transfer: None,
        };

        let mut dl = DisplayList::new(page_rect);
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: page_rect,
            alpha: 1.0,
            mask: Some(mask),
        });
        let mut full = zpdf_display_list::Path::new();
        full.rect(Rect::new(10.0, 10.0, 90.0, 90.0));
        dl.push(RenderCommand::FillPath {
            path: full,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push_pattern_range(1, 2);
        dl.push_expanded_glyph_run(
            1,
            2,
            GlyphRun {
                font_id,
                font_size: 40.0,
                glyphs: vec![PositionedGlyph {
                    glyph_id: 65,
                    code_len: 1,
                    code: 65,
                    x: 0.0,
                    y: 0.0,
                    advance: 40.0,
                }],
                paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
                alpha: 1.0,
                overprint: None,
                transform: Matrix::translate(10.0, 10.0),
                h_scale: 1.0,
            },
        );
        dl.push(RenderCommand::PopBlendGroup);

        let slide = display_list_to_slide(
            &dl,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");

        assert_eq!(
            slide.elements.len(),
            1,
            "pattern-range expanded glyph run must flatten as masked image with no TextBox: {:?}",
            slide.elements
        );
        let SlideElement::Image { image_data, .. } = &slide.elements[0] else {
            panic!("expected SlideElement::Image, got {:?}", slide.elements[0]);
        };
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        let (w, h) = rgba.dimensions();
        assert!(
            rgba.get_pixel(w / 2, h / 2).0[3] > 200,
            "left half revealed by mask must be opaque red"
        );
    }

    #[test]
    fn partially_mapped_expanded_run_inside_soft_mask_preserves_mask() {
        let mut char_procs = std::collections::HashMap::new();
        char_procs.insert(
            "A".to_string(),
            std::sync::Arc::from(b"1000 0 d0 0 0 800 800 re f".as_slice()),
        );
        char_procs.insert(
            "logo_custom".to_string(),
            std::sync::Arc::from(b"1000 0 d0 0 0 800 800 re f".as_slice()),
        );
        let mut encoding = vec![String::new(); 256];
        encoding[65] = "A".to_string();
        encoding[66] = "logo_custom".to_string();
        let t3 = LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [0.001, 0.0, 0.0, 0.001, 0.0, 0.0],
                char_procs,
                encoding,
                widths: vec![1000.0, 1000.0],
                first_char: 65,
            },
            "T3Partial".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(1000.0),
        );
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), t3);

        let page_rect = Rect::new(0.0, 0.0, 120.0, 80.0);
        // Alpha mask reveals only the left fill [10..50], hiding the right fill [70..110].
        let mut mask_dl = DisplayList::new(page_rect);
        let mut left_mask = zpdf_display_list::Path::new();
        left_mask.rect(Rect::new(0.0, 0.0, 55.0, 80.0));
        mask_dl.push(RenderCommand::FillPath {
            path: left_mask,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let mask = zpdf_display_list::SoftMask {
            kind: zpdf_display_list::SoftMaskKind::Alpha,
            commands: std::sync::Arc::new(mask_dl),
            offset: (0.0, 0.0),
            backdrop_luma: 0.0,
            transfer: None,
        };

        let mut dl = DisplayList::new(page_rect);
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: page_rect,
            alpha: 1.0,
            mask: Some(mask),
        });
        let mut left_fill = zpdf_display_list::Path::new();
        left_fill.rect(Rect::new(10.0, 20.0, 50.0, 60.0));
        dl.push(RenderCommand::FillPath {
            path: left_fill,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        let mut right_fill = zpdf_display_list::Path::new();
        right_fill.rect(Rect::new(70.0, 20.0, 110.0, 60.0));
        dl.push(RenderCommand::FillPath {
            path: right_fill,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push_expanded_glyph_run(
            1,
            3,
            GlyphRun {
                font_id,
                font_size: 40.0,
                glyphs: vec![
                    PositionedGlyph {
                        glyph_id: 65,
                        code_len: 1,
                        code: 65,
                        x: 0.0,
                        y: 0.0,
                        advance: 40.0,
                    },
                    PositionedGlyph {
                        glyph_id: 66,
                        code_len: 1,
                        code: 66,
                        x: 60.0,
                        y: 0.0,
                        advance: 40.0,
                    },
                ],
                paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
                alpha: 1.0,
                overprint: None,
                transform: Matrix::translate(10.0, 20.0),
                h_scale: 1.0,
            },
        );
        dl.push(RenderCommand::PopBlendGroup);

        let slide = display_list_to_slide(
            &dl,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");

        assert_eq!(
            slide.elements.len(),
            1,
            "rejected partially mapped expanded run must allow masked-group flattening: {:?}",
            slide.elements
        );
        let SlideElement::Image {
            width_emu,
            image_data,
            ..
        } = &slide.elements[0]
        else {
            panic!("expected SlideElement::Image, got {:?}", slide.elements[0]);
        };
        assert!(
            *width_emu <= pt_to_emu(50.0),
            "mask crops out the hidden right blue fill, expected width <= 50pt, got {width_emu}"
        );
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        assert!(
            rgba.pixels()
                .all(|p| p.0[3] == 0 || (p.0[0] > 200 && p.0[2] < 40)),
            "only the masked-in red fill may survive; hidden blue fill must not leak"
        );
    }

    #[test]
    fn wide_thin_crop_does_not_enlarge_slide_image_beyond_crop_bounds() {
        let page_rect = Rect::new(0.0, 0.0, 16_384.0, 12.0);
        let mut mask_dl = DisplayList::new(page_rect);
        let mut full = zpdf_display_list::Path::new();
        full.rect(page_rect);
        mask_dl.push(RenderCommand::FillPath {
            path: full.clone(),
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let mask = zpdf_display_list::SoftMask {
            kind: zpdf_display_list::SoftMaskKind::Alpha,
            commands: std::sync::Arc::new(mask_dl),
            offset: (0.0, 0.0),
            backdrop_luma: 0.0,
            transfer: None,
        };

        let mut dl = DisplayList::new(page_rect);
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: page_rect,
            alpha: 1.0,
            mask: Some(mask),
        });
        dl.push(RenderCommand::FillPath {
            path: full,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);

        let slide = display_list_to_slide(
            &dl,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");

        assert_eq!(slide.elements.len(), 1);
        let SlideElement::Image {
            width_emu,
            height_emu,
            image_data,
            ..
        } = &slide.elements[0]
        else {
            panic!("expected SlideElement::Image, got {:?}", slide.elements[0]);
        };
        assert_eq!(
            *width_emu,
            pt_to_emu(16_384.0),
            "aligned width must stay exact at 16,384 pt"
        );
        assert_eq!(
            *height_emu,
            pt_to_emu(12.0),
            "fractional pixel rounding must NOT enlarge 12 pt height to 16 pt"
        );
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        assert_eq!(rgba.dimensions(), (2048, 2));
        assert_eq!(
            rgba.get_pixel(1024, 0).0,
            [255, 0, 0, 255],
            "top row of resampled strip must stay opaque red"
        );
        assert_eq!(
            rgba.get_pixel(1024, 1).0,
            [255, 0, 0, 255],
            "bottom row of resampled strip must recover full opacity instead of 50% padding dilution"
        );
    }

    #[test]
    fn square_cap_diagonal_stroke_under_page_mask_keeps_projecting_cap_corners() {
        let page_rect = Rect::new(0.0, 0.0, 200.0, 200.0);
        let mut mask_dl = DisplayList::new(page_rect);
        let mut full = zpdf_display_list::Path::new();
        full.rect(page_rect);
        mask_dl.push(RenderCommand::FillPath {
            path: full,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let mask = zpdf_display_list::SoftMask {
            kind: zpdf_display_list::SoftMaskKind::Alpha,
            commands: std::sync::Arc::new(mask_dl),
            offset: (0.0, 0.0),
            backdrop_luma: 0.0,
            transfer: None,
        };

        let mut stroke_path = zpdf_display_list::Path::new();
        stroke_path.move_to(zpdf_core::Point::new(40.0, 40.0));
        stroke_path.line_to(zpdf_core::Point::new(70.0, 80.0));

        let mut dl = DisplayList::new(page_rect);
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: page_rect,
            alpha: 1.0,
            mask: Some(mask),
        });
        dl.push(RenderCommand::StrokePath {
            path: stroke_path,
            style: zpdf_display_list::StrokeStyle {
                width: 20.0,
                cap: zpdf_display_list::LineCap::Square,
                join: zpdf_display_list::LineJoin::Bevel,
                miter_limit: 10.0,
                dash: None,
            },
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);

        let slide = display_list_to_slide(
            &dl,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");

        assert_eq!(slide.elements.len(), 1);
        let SlideElement::Image {
            x_emu,
            y_emu,
            width_emu,
            height_emu,
            image_data,
            ..
        } = &slide.elements[0]
        else {
            panic!("expected SlideElement::Image, got {:?}", slide.elements[0]);
        };
        // Projecting square cap on (40,40)->(70,80) with width 20 extends to x in [26..84], y in [26..94].
        // In PPT coords (top-down from y=200): y_top <= 200 - 94 = 106, y_bottom >= 200 - 26 = 174.
        assert!(
            *x_emu <= pt_to_emu(26.0) && *x_emu + *width_emu >= pt_to_emu(84.0),
            "horizontal crop must cover projecting square caps [26..84], got x={x_emu} w={width_emu}"
        );
        assert!(
            *y_emu <= pt_to_emu(106.0) && *y_emu + *height_emu >= pt_to_emu(174.0),
            "vertical crop must cover projecting square caps y=[26..94] (ppt [106..174]), got y={y_emu} h={height_emu}"
        );

        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        let (w, h) = rgba.dimensions();
        // Check a point inside the projecting square cap corner beyond y=90 (specifically (68, 92) in PDF coords).
        let crop_x0 = *x_emu as f64 / 12700.0;
        let crop_y1 = 200.0 - (*y_emu as f64 / 12700.0);
        let crop_w = *width_emu as f64 / 12700.0;
        let crop_h = *height_emu as f64 / 12700.0;
        let px_x = (((68.0 - crop_x0) / crop_w) * w as f64) as u32;
        let px_y = (((crop_y1 - 92.0) / crop_h) * h as f64) as u32;
        let cap_px = rgba.get_pixel(px_x.min(w - 1), px_y.min(h - 1)).0;
        assert!(
            cap_px[3] > 200 && cap_px[2] > 200 && cap_px[0] < 40,
            "projecting square cap corner at PDF (68, 92) must survive unclipped, got {cap_px:?}"
        );
    }

    #[test]
    fn type3_mapped_d0_glyph_with_disjoint_clip_leaves_slide_blank_and_preserves_extraction() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // /A installs a clip [0..200] and fills [500..800] entirely outside it in caller's black.
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 120 100] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"0 0 0 rg BT /F1 40 Tf 10 30 Td (A) Tj ET"), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 65 /Widths [1000] /Resources << >> >>"
                .to_vec(), // 5
            b"<< /A 8 0 R >>".to_vec(),                    // 6
            b"<< /Type /Encoding /Differences [65 /A] >>".to_vec(), // 7
            stream(
                "",
                b"1000 0 d0 0 0 200 200 re W n 0 0 0 rg 500 500 300 300 re f",
            ), // 8
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let mut spans = Vec::new();
        let interpreter = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .with_text_sink(&mut spans);
        let (display_list, _) = interpreter.interpret_with_stats(&content_bytes);

        let extracted: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(
            extracted, "A",
            "disjoint clipped Type 3 glyph must remain extractable as text"
        );

        let slide = display_list_to_slide(
            &display_list,
            0,
            &fonts,
            &images,
            &ConversionOptions::default(),
        )
        .expect("slide export");
        assert!(
            slide.elements.is_empty(),
            "disjoint clip and fill in mapped CharProc must leave the slide blank, got {:?}",
            slide.elements
        );
    }

    #[test]
    fn type3_d0_fill_and_stroke_opacity_preserved_in_pptx() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // /A: /ca 0 fill (invisible)
        // /B: /CA 0 stroke (invisible)
        // /C: /ca 0.5 red fill (translucent)
        // /D: /CA 0.5 blue horizontal stroke (translucent)
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 100] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"0 0 0 rg BT /F1 40 Tf 10 30 Td (ABCD) Tj ET"), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 68 /Widths [1000 1000 1000 1000] \
              /Resources << /ExtGState << /GS0 12 0 R /GS50 13 0 R >> >> >>"
                .to_vec(), // 5
            b"<< /A 8 0 R /B 9 0 R /C 10 0 R /D 11 0 R >>".to_vec(), // 6
            b"<< /Type /Encoding /Differences [65 /A /B /C /D] >>".to_vec(), // 7
            stream("", b"1000 0 d0 /GS0 gs 1 0 0 rg 0 0 800 800 re f"), // 8 (ca=0 fill)
            stream("", b"1000 0 d0 /GS0 gs 0 0 1 RG 200 w 0 400 m 800 400 l S"), // 9 (CA=0 stroke)
            stream("", b"1000 0 d0 /GS50 gs 1 0 0 rg 0 0 800 800 re f"), // 10 (ca=0.5 red fill)
            stream("", b"1000 0 d0 /GS50 gs 0 0 1 RG 200 w 0 400 m 800 400 l S"), // 11 (CA=0.5 blue stroke)
            b"<< /Type /ExtGState /ca 0 /CA 0 >>".to_vec(),                       // 12
            b"<< /Type /ExtGState /ca 0.5 /CA 0.5 >>".to_vec(),                   // 13
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let interpreter = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images);
        let (display_list, _) = interpreter.interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(
            &display_list,
            0,
            &fonts,
            &images,
            &ConversionOptions::default(),
        )
        .expect("slide export");

        // Zero-opacity /A and /B must be omitted; translucent /C and /D must export as 2 Images.
        assert_eq!(
            slide.elements.len(),
            2,
            "transparent /ca 0 & /CA 0 must emit nothing, leaving only translucent /C and /D: {:?}",
            slide.elements
        );
        let sample_center = |el: &SlideElement| -> [u8; 4] {
            let SlideElement::Image { image_data, .. } = el else {
                panic!("translucent CharProc paint must export as SlideElement::Image, got {el:?}");
            };
            let rgba = image::load_from_memory(image_data)
                .expect("decode PNG")
                .to_rgba8();
            let (w, h) = rgba.dimensions();
            rgba.get_pixel(w / 2, h / 2).0
        };
        let fill_px = sample_center(&slide.elements[0]);
        assert!(
            fill_px[0] > 240
                && fill_px[1] < 15
                && fill_px[2] < 15
                && (120..=136).contains(&fill_px[3]),
            "translucent /ca 0.5 red fill must keep ~50% alpha, got {fill_px:?}"
        );
        let stroke_px = sample_center(&slide.elements[1]);
        assert!(
            stroke_px[2] > 240
                && stroke_px[0] < 15
                && stroke_px[1] < 15
                && (120..=136).contains(&stroke_px[3]),
            "translucent /CA 0.5 blue stroke must keep ~50% alpha, got {stroke_px:?}"
        );
    }

    #[test]
    fn wide_thin_crop_preserves_anisotropic_stroke_width_and_dash_geometry() {
        let page_rect = Rect::new(0.0, 0.0, 16_384.0, 12.0);
        let mut mask_dl = DisplayList::new(page_rect);
        let mut full = zpdf_display_list::Path::new();
        full.rect(page_rect);
        mask_dl.push(RenderCommand::FillPath {
            path: full.clone(),
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let mask = zpdf_display_list::SoftMask {
            kind: zpdf_display_list::SoftMaskKind::Alpha,
            commands: std::sync::Arc::new(mask_dl),
            offset: (0.0, 0.0),
            backdrop_luma: 0.0,
            transfer: None,
        };

        let mut hline = zpdf_display_list::Path::new();
        hline.move_to(zpdf_core::Point::new(0.0, 6.0));
        hline.line_to(zpdf_core::Point::new(16_384.0, 6.0));
        let style = zpdf_display_list::StrokeStyle {
            width: 12.0,
            cap: zpdf_display_list::LineCap::Butt,
            join: zpdf_display_list::LineJoin::Miter,
            miter_limit: 10.0,
            dash: Some(zpdf_display_list::DashPattern {
                array: vec![64.0, 64.0],
                phase: 0.0,
            }),
        };

        for is_clip_stroke in [false, true] {
            let mut dl = DisplayList::new(page_rect);
            dl.push(RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: page_rect,
                alpha: 1.0,
                mask: Some(mask.clone()),
            });
            if is_clip_stroke {
                dl.push(RenderCommand::PushClipStroke {
                    path: hline.clone(),
                    style: style.clone(),
                });
                dl.push(RenderCommand::FillPath {
                    path: full.clone(),
                    rule: zpdf_display_list::FillRule::NonZero,
                    paint: Paint::Solid(Color::rgb(0.0, 1.0, 0.0)),
                    alpha: 1.0,
                    overprint: None,
                });
                dl.push(RenderCommand::PopClip);
            } else {
                dl.push(RenderCommand::StrokePath {
                    path: hline.clone(),
                    style: style.clone(),
                    paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
                    alpha: 1.0,
                    overprint: None,
                });
            }
            dl.push(RenderCommand::PopBlendGroup);

            let slide = display_list_to_slide(
                &dl,
                0,
                &FontCache::new(),
                &ImageCache::new(),
                &ConversionOptions::default(),
            )
            .expect("slide export");

            assert_eq!(slide.elements.len(), 1);
            let SlideElement::Image {
                width_emu,
                height_emu,
                image_data,
                ..
            } = &slide.elements[0]
            else {
                panic!("expected SlideElement::Image, got {:?}", slide.elements[0]);
            };
            assert_eq!(*width_emu, pt_to_emu(16_384.0));
            assert_eq!(*height_emu, pt_to_emu(12.0));

            let rgba = image::load_from_memory(image_data)
                .expect("decode PNG")
                .to_rgba8();
            assert_eq!(rgba.dimensions(), (2048, 2));

            let expected_on = if is_clip_stroke {
                [0, 255, 0, 255]
            } else {
                [0, 0, 255, 255]
            };
            // At scale 1/8 (8 pt/px), 64 pt dashes occupy 8 px ON, 8 px OFF.
            // Check well into the pattern (px=36 -> x=288 ON; px=28 -> x=224 OFF)
            // where geometric-mean scaling (~1.155x) would both phase-drift dashes
            // and thin the 12 pt stroke below full opacity.
            for row in 0..2 {
                assert_eq!(
                    rgba.get_pixel(4, row).0,
                    expected_on,
                    "first dash ON pixel (row={row}, clip={is_clip_stroke}) must be fully opaque"
                );
                assert_eq!(
                    rgba.get_pixel(12, row).0[3],
                    0,
                    "first dash OFF gap (row={row}, clip={is_clip_stroke}) must stay transparent"
                );
                assert_eq!(
                    rgba.get_pixel(28, row).0[3],
                    0,
                    "second dash OFF gap at x=224pt (row={row}, clip={is_clip_stroke}) must not drift"
                );
                assert_eq!(
                    rgba.get_pixel(36, row).0,
                    expected_on,
                    "third dash ON pixel at x=288pt (row={row}, clip={is_clip_stroke}) must keep full height & phase"
                );
            }
        }
    }

    #[test]
    fn type3_painted_whitespace_glyph_keeps_drawing_while_blank_space_stays_editable() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Code 32 (/sp_mark): /ToUnicode -> U+0020, paints a visible caller-colored square.
        // Code 33 (/sp_blank): /ToUnicode -> U+0020, ordinary metrics-only blank space.
        // Codes 65 (/A) and 66 (/B): painted letters.
        let to_unicode = b"/CIDInit /ProcSet findresource begin\n\
            12 dict begin begincmap\n\
            /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
            /CMapName /Test def /CMapType 2 def\n\
            1 begincodespacerange <00> <FF> endcodespacerange\n\
            4 beginbfchar\n\
            <20> <0020>\n\
            <21> <0020>\n\
            <41> <0041>\n\
            <42> <0042>\n\
            endbfchar\n\
            endcmap CMapName currentdict /CMap defineresource pop end end";

        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 260 100] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"0 0 0 rg BT /F1 40 Tf 10 30 Td (A !B) Tj ET"), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /ToUnicode 12 0 R \
              /FirstChar 32 /LastChar 66 /Widths [1000 1000 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 1000 1000] \
              /Resources << >> >>"
                .to_vec(), // 5
            b"<< /sp_mark 8 0 R /sp_blank 9 0 R /A 10 0 R /B 11 0 R >>".to_vec(), // 6
            b"<< /Type /Encoding /Differences [32 /sp_mark /sp_blank 65 /A /B] >>".to_vec(), // 7
            stream("", b"1000 0 d0 0 0 0 rg 100 100 600 600 re f"), // 8 (painted U+0020)
            stream("", b"1000 0 d0"),                               // 9 (blank U+0020)
            stream("", b"1000 0 d0 0 0 0 rg 0 0 800 800 re f"),     // 10 (/A)
            stream("", b"1000 0 d0 0 0 0 rg 0 0 800 800 re f"),     // 11 (/B)
            stream("", to_unicode),                                 // 12
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let mut spans = Vec::new();
        let interpreter = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .with_text_sink(&mut spans);
        let (display_list, _) = interpreter.interpret_with_stats(&content_bytes);

        let extracted: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(
            extracted, "A  B",
            "text extraction must retain logical spaces for both U+0020 glyphs"
        );

        let slide = display_list_to_slide(
            &display_list,
            0,
            &fonts,
            &images,
            &ConversionOptions::default(),
        )
        .expect("slide export");

        assert!(
            slide.elements.iter().any(|e| matches!(
                e,
                SlideElement::Rectangle {
                    fill_rgb: Some((0, 0, 0)),
                    ..
                }
            )),
            "painted U+0020 glyph must keep its drawn artwork instead of becoming a blank space: {:?}",
            slide.elements
        );
        let boxes: Vec<(&str, i64, i64)> = slide
            .elements
            .iter()
            .filter_map(|e| match e {
                SlideElement::TextBox {
                    text,
                    x_emu,
                    width_emu,
                    ..
                } => Some((text.as_str(), *x_emu, *width_emu)),
                _ => None,
            })
            .collect();
        assert_eq!(
            boxes,
            vec![
                ("A", pt_to_emu(10.0), pt_to_emu(40.0)),
                (" B", pt_to_emu(90.0), pt_to_emu(80.0)),
            ],
            "blank U+0020 space must remain editable with its advance while painted U+0020 stays artwork"
        );
    }

    #[test]
    fn type3_page_opacity_before_text_show_preserved_in_pptx() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Page sets /ca 0 before (A), /ca 0.5 before (B), and /ca 1 before (C).
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 100] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> \
              /ExtGState << /GS0 11 0 R /GS50 12 0 R /GS100 13 0 R >> >> >>"
                .to_vec(), // 3
            stream(
                "",
                b"1 0 0 rg /GS0 gs BT /F1 40 Tf 10 30 Td (A) Tj ET \
                  /GS50 gs BT /F1 40 Tf 70 30 Td (B) Tj ET \
                  /GS100 gs BT /F1 40 Tf 130 30 Td (C) Tj ET",
            ), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 67 /Widths [1000 1000 1000] /Resources << >> >>"
                .to_vec(), // 5
            b"<< /A 8 0 R /B 9 0 R /C 10 0 R >>".to_vec(), // 6
            b"<< /Type /Encoding /Differences [65 /A /B /C] >>".to_vec(), // 7
            stream("", b"1000 0 0 0 1000 1000 d1 0 0 800 800 re f"), // 8
            stream("", b"1000 0 0 0 1000 1000 d1 0 0 800 800 re f"), // 9
            stream("", b"1000 0 0 0 1000 1000 d1 0 0 800 800 re f"), // 10
            b"<< /Type /ExtGState /ca 0 >>".to_vec(),      // 11
            b"<< /Type /ExtGState /ca 0.5 >>".to_vec(),    // 12
            b"<< /Type /ExtGState /ca 1 >>".to_vec(),      // 13
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let mut spans = Vec::new();
        let interpreter = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .with_text_sink(&mut spans);
        let (display_list, _) = interpreter.interpret_with_stats(&content_bytes);

        let extracted: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(extracted, "ABC");

        let slide = display_list_to_slide(
            &display_list,
            0,
            &fonts,
            &images,
            &ConversionOptions::default(),
        )
        .expect("slide export");

        assert_eq!(
            slide.elements.len(),
            2,
            "/ca 0 letter (A) must be omitted, leaving translucent (B) image and opaque (C) text: {:?}",
            slide.elements
        );
        let SlideElement::Image { image_data, .. } = &slide.elements[0] else {
            panic!(
                "/ca 0.5 letter (B) must export as translucent Image, got {:?}",
                slide.elements[0]
            );
        };
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        let (w, h) = rgba.dimensions();
        let center = rgba.get_pixel(w / 2, h / 2).0;
        assert!(
            center[0] > 240 && center[1] < 15 && center[2] < 15 && (120..=136).contains(&center[3]),
            "translucent /ca 0.5 letter (B) must preserve ~50% red alpha, got {center:?}"
        );
        match &slide.elements[1] {
            SlideElement::TextBox { text, .. } => assert_eq!(text, "C"),
            other => panic!("opaque /ca 1 letter (C) must remain editable TextBox, got {other:?}"),
        }
    }

    #[test]
    fn zero_and_translucent_transparency_forms_apply_group_opacity_to_editable_text_and_shapes() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Form XObject with `/Group /S /Transparency` draws a mapped Type 3 letter (A)
        // AND a sibling red rectangle.
        let form = stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 160 100] \
             /Group << /Type /Group /S /Transparency /CS /DeviceRGB /I true >> \
             /Resources << /Font << /F1 5 0 R >> >>",
            b"0 0 0 rg BT /F1 40 Tf 10 30 Td (A) Tj ET 1 0 0 rg 80 30 40 40 re f",
        );

        let build_pdf = |gs_dict: &[u8]| {
            assemble(&[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 160 100] /Contents 4 0 R \
                  /Resources << /XObject << /Fm 9 0 R >> /ExtGState << /GS 10 0 R >> >> >>"
                    .to_vec(), // 3
                stream("", b"/GS gs /Fm Do"),                  // 4
                b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
                  /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
                  /FirstChar 65 /LastChar 65 /Widths [1000] /Resources << >> >>"
                    .to_vec(), // 5
                b"<< /A 8 0 R >>".to_vec(),                    // 6
                b"<< /Type /Encoding /Differences [65 /A] >>".to_vec(), // 7
                stream("", b"1000 0 d0 0 0 800 800 re f"),     // 8
                form.clone(),                                  // 9
                gs_dict.to_vec(),                              // 10
            ])
        };

        // Zero-alpha Form: must produce NO visible slide elements.
        let zero_pdf = build_pdf(b"<< /Type /ExtGState /ca 0 >>");
        let doc = zpdf_document::PdfDocument::open(zero_pdf).expect("open zero pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content_bytes);
        let zero_slide =
            display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
                .expect("zero slide");
        assert!(
            zero_slide.elements.is_empty(),
            "zero-alpha transparency Form with letter and rectangle must produce no visible slide elements, got {:?}",
            zero_slide.elements
        );

        // 50%-alpha Form: must flatten the entire group at ~50% opacity (no opaque TextBox/Rect).
        let half_pdf = build_pdf(b"<< /Type /ExtGState /ca 0.5 >>");
        let doc = zpdf_document::PdfDocument::open(half_pdf).expect("open half pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content_bytes);
        let half_slide =
            display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
                .expect("half slide");
        assert_eq!(
            half_slide.elements.len(),
            1,
            "translucent transparency Form must flatten to one translucent Image, got {:?}",
            half_slide.elements
        );
        let SlideElement::Image { image_data, .. } = &half_slide.elements[0] else {
            panic!(
                "expected translucent SlideElement::Image, got {:?}",
                half_slide.elements[0]
            );
        };
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        let (w, h) = rgba.dimensions();
        let letter_px = rgba.get_pixel(w / 5, h / 2).0;
        let rect_px = rgba.get_pixel(w * 4 / 5, h / 2).0;
        assert!(
            (120..=136).contains(&letter_px[3]) && letter_px[0] < 15 && letter_px[2] < 15,
            "letter inside 50%-alpha Form must have ~50% black opacity, got {letter_px:?}"
        );
        assert!(
            (120..=136).contains(&rect_px[3]) && rect_px[0] > 240 && rect_px[2] < 15,
            "rectangle inside 50%-alpha Form must have ~50% red opacity, got {rect_px:?}"
        );
    }

    #[test]
    fn non_normal_multiply_blend_preserves_backdrop_color_in_pptx() {
        let page_rect = Rect::new(0.0, 0.0, 140.0, 140.0);
        let mut dl = DisplayList::new(page_rect);
        let mut blue_rect = zpdf_display_list::Path::new();
        blue_rect.rect(Rect::new(10.0, 10.0, 90.0, 90.0));
        dl.push(RenderCommand::FillPath {
            path: blue_rect,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: page_rect,
            alpha: 1.0,
            mask: None,
        });
        let mut red_rect = zpdf_display_list::Path::new();
        red_rect.rect(Rect::new(40.0, 40.0, 120.0, 120.0));
        dl.push(RenderCommand::FillPath {
            path: red_rect,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);

        let slide = display_list_to_slide(
            &dl,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");

        assert_eq!(
            slide.elements.len(),
            2,
            "expected blue Rectangle backdrop plus blended Image, got {:?}",
            slide.elements
        );
        assert!(matches!(
            slide.elements[0],
            SlideElement::Rectangle {
                fill_rgb: Some((0, 0, 255)),
                ..
            }
        ));
        let SlideElement::Image { image_data, .. } = &slide.elements[1] else {
            panic!(
                "expected blended SlideElement::Image, got {:?}",
                slide.elements[1]
            );
        };
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        let (w, h) = rgba.dimensions();
        // Bottom-left quadrant of the red [40..120] crop overlaps blue [10..90] -> Multiply(red, blue) = black.
        let overlap_px = rgba.get_pixel(w / 4, h * 3 / 4).0;
        assert_eq!(
            overlap_px,
            [0, 0, 0, 255],
            "red Multiply over opaque blue backdrop must produce black overlap, got {overlap_px:?}"
        );
        // Top-right quadrant [90..120] is over white backdrop -> Multiply(red, white) = red.
        let non_overlap_px = rgba.get_pixel(w * 3 / 4, h / 4).0;
        assert_eq!(
            non_overlap_px,
            [255, 0, 0, 255],
            "red Multiply over white backdrop must stay red, got {non_overlap_px:?}"
        );
        // Outer padding corner outside [40..120] must remain transparent so backdrop shapes stay untouched.
        assert_eq!(
            rgba.get_pixel(0, 0).0[3],
            0,
            "padding outside the blended shape must stay transparent"
        );
    }

    #[test]
    fn unmapped_type3_opaque_icon_using_nested_font_exports_as_artwork_not_text() {
        const BOLD_TTF: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/coverage-bold.ttf");
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Unmapped Type 3 glyph `/icon_custom` (no /ToUnicode) draws its opaque shape
        // using an embedded ordinary TrueType font with no soft mask.
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 120 120] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"0 0 0 rg BT /F1 60 Tf 20 20 Td (A) Tj ET"), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 65 /Widths [1000] \
              /Resources << /Font << /FNest 9 0 R >> >> >>"
                .to_vec(), // 5
            b"<< /icon_custom 8 0 R >>".to_vec(),          // 6
            b"<< /Type /Encoding /Differences [65 /icon_custom] >>".to_vec(), // 7
            stream("", b"1000 0 d0 1 0 0 rg BT /FNest 900 Tf 0 0 Td (A) Tj ET"), // 8
            b"<< /Type /Font /Subtype /TrueType /BaseFont /BoldSquare \
              /FirstChar 65 /LastChar 65 /Widths [600] /Encoding /WinAnsiEncoding \
              /FontDescriptor 10 0 R >>"
                .to_vec(), // 9
            b"<< /Type /FontDescriptor /FontName /BoldSquare /Flags 32 \
              /FontBBox [0 0 1000 1000] /ItalicAngle 0 /Ascent 800 /Descent -200 \
              /CapHeight 700 /StemV 80 /FontFile2 11 0 R >>"
                .to_vec(), // 10
            stream(&format!("/Length1 {}", BOLD_TTF.len()), BOLD_TTF), // 11
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
            .expect("slide export");

        assert!(
            !slide
                .elements
                .iter()
                .any(|e| matches!(e.base(), SlideElement::TextBox { .. })),
            "unmapped Type 3 icon using nested font must NOT leak as editable TextBox: {:?}",
            slide.elements
        );
        assert_eq!(slide.elements.len(), 1);
        let SlideElement::Image { image_data, .. } = &slide.elements[0] else {
            panic!("expected SlideElement::Image, got {:?}", slide.elements[0]);
        };
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        assert!(
            rgba.pixels()
                .any(|p| p.0[3] > 200 && p.0[0] > 200 && p.0[2] < 40),
            "unmapped Type 3 nested-font icon must preserve its opaque red glyph artwork"
        );
    }

    #[test]
    fn ordinary_translucent_rectangle_remains_editable_shape() {
        let page_rect = Rect::new(0.0, 0.0, 200.0, 100.0);
        let mut dl = DisplayList::new(page_rect);
        let mut rect_path = zpdf_display_list::Path::new();
        rect_path.rect(Rect::new(20.0, 20.0, 80.0, 70.0));
        dl.push(RenderCommand::FillPath {
            path: rect_path,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 0.5,
            overprint: None,
        });
        let mut line_path = zpdf_display_list::Path::new();
        line_path.move_to(zpdf_core::Point::new(100.0, 20.0));
        line_path.line_to(zpdf_core::Point::new(180.0, 70.0));
        dl.push(RenderCommand::StrokePath {
            path: line_path,
            style: zpdf_display_list::StrokeStyle {
                width: 4.0,
                cap: zpdf_display_list::LineCap::Butt,
                join: zpdf_display_list::LineJoin::Miter,
                miter_limit: 10.0,
                dash: None,
            },
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 0.5,
            overprint: None,
        });

        let slide = display_list_to_slide(
            &dl,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");

        assert_eq!(slide.elements.len(), 2);
        match &slide.elements[0] {
            SlideElement::Opacity { element, alpha } => {
                assert!(
                    (*alpha - 0.5).abs() < 1e-3,
                    "translucent rectangle must preserve 0.5 opacity, got {alpha}"
                );
                assert!(
                    matches!(
                        element.as_ref(),
                        SlideElement::Rectangle {
                            fill_rgb: Some((255, 0, 0)),
                            ..
                        }
                    ),
                    "ordinary translucent rectangle must stay native editable SlideElement::Rectangle, got {element:?}"
                );
            }
            other => panic!(
                "ordinary translucent rectangle must preserve opacity on native SlideElement::Rectangle, got {other:?}"
            ),
        }
        match &slide.elements[1] {
            SlideElement::Opacity { element, alpha } => {
                assert!(
                    (*alpha - 0.5).abs() < 1e-3,
                    "translucent line must preserve 0.5 opacity, got {alpha}"
                );
                assert!(
                    matches!(
                        element.as_ref(),
                        SlideElement::Line {
                            stroke_rgb: (0, 0, 255),
                            ..
                        }
                    ),
                    "ordinary translucent line must stay native editable SlideElement::Line, got {element:?}"
                );
            }
            other => panic!(
                "ordinary translucent line must preserve opacity on native SlideElement::Line, got {other:?}"
            ),
        }

        let bytes = PptxWriter::new(PptxPresentation {
            slides: vec![slide],
            width_emu: pt_to_emu(200.0),
            height_emu: pt_to_emu(100.0),
        })
        .write_to_bytes()
        .expect("write pptx");
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
        let xml = String::from_utf8(zip_entry(&mut archive, "ppt/slides/slide1.xml")).unwrap();
        assert!(
            xml.contains(r#"<a:srgbClr val="FF0000"><a:alpha val="50000"/></a:srgbClr>"#),
            "rectangle XML must carry 50% fill alpha: {xml}"
        );
        assert!(
            xml.contains(r#"<a:srgbClr val="0000FF"><a:alpha val="50000"/></a:srgbClr>"#),
            "line XML must carry 50% stroke alpha: {xml}"
        );
    }

    #[test]
    fn wide_thin_crop_preserves_one_device_pixel_hairline_strokes_and_clips() {
        let page_rect = Rect::new(0.0, 0.0, 16_384.0, 12.0);
        let mut mask_dl = DisplayList::new(page_rect);
        let mut full = zpdf_display_list::Path::new();
        full.rect(page_rect);
        mask_dl.push(RenderCommand::FillPath {
            path: full.clone(),
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let mask = zpdf_display_list::SoftMask {
            kind: zpdf_display_list::SoftMaskKind::Alpha,
            commands: std::sync::Arc::new(mask_dl),
            offset: (0.0, 0.0),
            backdrop_luma: 0.0,
            transfer: None,
        };

        // Anchor the full 16,384 × 12 crop with an opaque dot at the far right edge,
        // and place a vertical zero-width hairline at x=12pt (exact center of pixel column 1
        // at scale 1/8 = 8pt/px).
        let mut vline = zpdf_display_list::Path::new();
        vline.move_to(zpdf_core::Point::new(12.0, 0.0));
        vline.line_to(zpdf_core::Point::new(12.0, 12.0));
        let mut far_right = zpdf_display_list::Path::new();
        far_right.rect(Rect::new(16_368.0, 0.0, 16_384.0, 12.0));
        let hairline_style = zpdf_display_list::StrokeStyle {
            width: 0.0,
            cap: zpdf_display_list::LineCap::Butt,
            join: zpdf_display_list::LineJoin::Miter,
            miter_limit: 10.0,
            dash: None,
        };

        // 1. Vertical zero-width StrokePath and PushClipStroke must cover 1 full device pixel on X.
        for is_clip_stroke in [false, true] {
            let mut dl = DisplayList::new(page_rect);
            dl.push(RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: page_rect,
                alpha: 1.0,
                mask: Some(mask.clone()),
            });
            // Keep crop extent fixed at full [0..16384] × [0..12].
            let mut far_left = zpdf_display_list::Path::new();
            far_left.rect(Rect::new(0.0, 0.0, 4.0, 12.0));
            dl.push(RenderCommand::FillPath {
                path: far_left,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
                alpha: 1.0,
                overprint: None,
            });
            dl.push(RenderCommand::FillPath {
                path: far_right.clone(),
                rule: zpdf_display_list::FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
                alpha: 1.0,
                overprint: None,
            });
            if is_clip_stroke {
                dl.push(RenderCommand::PushClipStroke {
                    path: vline.clone(),
                    style: hairline_style.clone(),
                });
                dl.push(RenderCommand::FillPath {
                    path: full.clone(),
                    rule: zpdf_display_list::FillRule::NonZero,
                    paint: Paint::Solid(Color::rgb(0.0, 1.0, 0.0)),
                    alpha: 1.0,
                    overprint: None,
                });
                dl.push(RenderCommand::PopClip);
            } else {
                dl.push(RenderCommand::StrokePath {
                    path: vline.clone(),
                    style: hairline_style.clone(),
                    paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
                    alpha: 1.0,
                    overprint: None,
                });
            }
            dl.push(RenderCommand::PopBlendGroup);

            let slide = display_list_to_slide(
                &dl,
                0,
                &FontCache::new(),
                &ImageCache::new(),
                &ConversionOptions::default(),
            )
            .expect("slide export");
            let SlideElement::Image { image_data, .. } = &slide.elements[0] else {
                panic!("expected SlideElement::Image");
            };
            let rgba = image::load_from_memory(image_data)
                .expect("decode PNG")
                .to_rgba8();
            assert_eq!(rgba.dimensions(), (2048, 2));
            let expected = if is_clip_stroke {
                [0, 255, 0, 255]
            } else {
                [0, 0, 255, 255]
            };
            for row in 0..2 {
                assert_eq!(
                    rgba.get_pixel(1, row).0,
                    expected,
                    "vertical 0-width hairline (row={row}, clip={is_clip_stroke}) must occupy 1 full device pixel (alpha=255)"
                );
                assert_eq!(
                    rgba.get_pixel(2, row).0[3],
                    0,
                    "neighboring column outside vertical hairline must stay transparent"
                );
            }
        }

        // 2. Horizontal zero-width dashed hairline at y=9pt (center of row 0) must keep 1px height & exact 64pt X dashes.
        let mut hline = zpdf_display_list::Path::new();
        hline.move_to(zpdf_core::Point::new(0.0, 9.0));
        hline.line_to(zpdf_core::Point::new(16_384.0, 9.0));
        let dashed_hairline = zpdf_display_list::StrokeStyle {
            width: 0.0,
            cap: zpdf_display_list::LineCap::Butt,
            join: zpdf_display_list::LineJoin::Miter,
            miter_limit: 10.0,
            dash: Some(zpdf_display_list::DashPattern {
                array: vec![64.0, 64.0],
                phase: 0.0,
            }),
        };
        let mut dl = DisplayList::new(page_rect);
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: page_rect,
            alpha: 1.0,
            mask: Some(mask),
        });
        dl.push(RenderCommand::FillPath {
            path: far_right,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::StrokePath {
            path: hline,
            style: dashed_hairline,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);

        let slide = display_list_to_slide(
            &dl,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");
        let SlideElement::Image { image_data, .. } = &slide.elements[0] else {
            panic!("expected SlideElement::Image");
        };
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        assert_eq!(rgba.dimensions(), (2048, 2));
        assert_eq!(rgba.get_pixel(4, 0).0, [0, 0, 255, 255]);
        assert_eq!(rgba.get_pixel(12, 0).0[3], 0);
        assert_eq!(rgba.get_pixel(28, 0).0[3], 0);
        assert_eq!(rgba.get_pixel(36, 0).0, [0, 0, 255, 255]);
        assert_eq!(
            rgba.get_pixel(4, 1).0[3],
            0,
            "horizontal 0-width hairline centered in row 0 must stay 1 device pixel tall"
        );
    }

    #[test]
    fn soft_masked_transparency_form_applies_mask_to_editable_text_and_shapes() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Form XObject with normal opacity draws a mapped Type 3 letter (A)
        // at [10..42] and a sibling red rectangle at [80..120].
        let content_form = stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 160 100] \
             /Group << /Type /Group /S /Transparency /CS /DeviceRGB /I true >> \
             /Resources << /Font << /F1 5 0 R >> >>",
            b"0 0 0 rg BT /F1 40 Tf 10 30 Td (A) Tj ET 1 0 0 rg 80 30 40 40 re f",
        );

        let build_pdf = |mask_ops: &[u8]| {
            assemble(&[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 160 100] /Contents 4 0 R \
                  /Resources << /XObject << /Fm 9 0 R >> /ExtGState << /GSMask 10 0 R >> >> >>"
                    .to_vec(), // 3
                stream("", b"/GSMask gs /Fm Do"),              // 4
                b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
                  /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
                  /FirstChar 65 /LastChar 65 /Widths [1000] /Resources << >> >>"
                    .to_vec(), // 5
                b"<< /A 8 0 R >>".to_vec(),                    // 6
                b"<< /Type /Encoding /Differences [65 /A] >>".to_vec(), // 7
                stream("", b"1000 0 d0 0 0 800 800 re f"),     // 8
                content_form.clone(),                          // 9
                b"<< /Type /ExtGState /SMask << /Type /Mask /S /Alpha /G 11 0 R >> >>".to_vec(), // 10
                stream(
                    "/Type /XObject /Subtype /Form /BBox [0 0 160 100] \
                     /Group << /Type /Group /S /Transparency /CS /DeviceRGB >>",
                    mask_ops,
                ), // 11
            ])
        };

        // Case 1: fully transparent alpha mask -> slide must be completely blank.
        let hidden_pdf = build_pdf(b"");
        let doc = zpdf_document::PdfDocument::open(hidden_pdf).expect("open hidden pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content_bytes);
        let hidden_slide =
            display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
                .expect("hidden slide");
        assert!(
            hidden_slide.elements.is_empty(),
            "Form under fully transparent soft mask containing mapped Type 3 letter and rectangle must leave slide blank, got {:?}",
            hidden_slide.elements
        );

        // Case 2: partial alpha mask reveals only left half [0..60] (the letter) and hides the red rectangle [80..120].
        let partial_pdf = build_pdf(b"1 1 1 rg 0 0 60 100 re f");
        let doc = zpdf_document::PdfDocument::open(partial_pdf).expect("open partial pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content_bytes);
        let partial_slide =
            display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
                .expect("partial slide");
        assert_eq!(
            partial_slide.elements.len(),
            1,
            "partially masked Form must flatten to one Image with no unmasked TextBox or Rectangle, got {:?}",
            partial_slide.elements
        );
        let SlideElement::Image {
            width_emu,
            image_data,
            ..
        } = &partial_slide.elements[0]
        else {
            panic!(
                "expected SlideElement::Image, got {:?}",
                partial_slide.elements[0]
            );
        };
        assert!(
            *width_emu <= pt_to_emu(62.0),
            "masked crop must exclude the hidden right rectangle, got width_emu={width_emu}"
        );
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        assert!(
            rgba.pixels()
                .all(|p| p.0[3] == 0 || (p.0[0] < 30 && p.0[1] < 30 && p.0[2] < 30)),
            "only the black masked-in letter may survive; red rectangle must stay masked out"
        );
    }

    #[test]
    fn type3_page_level_clip_cuts_hides_or_preserves_mapped_letter() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Mapped /A fills [0..800]x[0..800] -> at 40pt at (10,30), painted rect is [10..42]x[30..62].
        let run_case = |page_ops: &[u8]| -> (String, PptxSlide) {
            let pdf = assemble(&[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 160 100] /Contents 4 0 R \
                  /Resources << /Font << /F1 5 0 R >> >> >>"
                    .to_vec(), // 3
                stream("", page_ops),                          // 4
                b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
                  /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
                  /FirstChar 65 /LastChar 65 /Widths [1000] /Resources << >> >>"
                    .to_vec(), // 5
                b"<< /A 8 0 R >>".to_vec(),                    // 6
                b"<< /Type /Encoding /Differences [65 /A] >>".to_vec(), // 7
                stream("", b"1000 0 d0 0 0 800 800 re f"),     // 8
            ]);
            let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
            let page = doc.page(0).expect("page 0");
            let mut fonts = doc.load_page_fonts(&page);
            let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
            let mut images = ImageCache::new();
            let mut spans = Vec::new();
            let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
                .with_document(doc.file(), &page.resources)
                .with_fonts(&mut fonts)
                .with_images(&mut images)
                .with_text_sink(&mut spans)
                .interpret_with_stats(&content_bytes);
            let extracted: String = spans.iter().map(|s| s.text.as_str()).collect();
            let slide =
                display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
                    .expect("slide export");
            (extracted, slide)
        };

        // 1. Disjoint page clip [100..150] fully hides the letter at [10..42].
        let (extracted, hidden_slide) =
            run_case(b"q 100 0 50 100 re W n 0 0 0 rg BT /F1 40 Tf 10 30 Td (A) Tj ET Q");
        assert_eq!(
            extracted, "A",
            "logical text must remain extractable when clipped out"
        );
        assert!(
            hidden_slide.elements.is_empty(),
            "disjoint page clip must hide mapped Type 3 letter completely, got {:?}",
            hidden_slide.elements
        );

        // 2. Partial page clip [0..26] cuts halfway through the letter at [10..42].
        let (extracted, cut_slide) =
            run_case(b"q 0 0 26 100 re W n 0 0 0 rg BT /F1 40 Tf 10 30 Td (A) Tj ET Q");
        assert_eq!(
            extracted, "A",
            "logical text must remain extractable when partially clipped"
        );
        assert_eq!(
            cut_slide.elements.len(),
            1,
            "partially clipped letter must export as one clipped Image, got {:?}",
            cut_slide.elements
        );
        let SlideElement::Image {
            x_emu,
            width_emu,
            image_data,
            ..
        } = &cut_slide.elements[0]
        else {
            panic!(
                "expected clipped SlideElement::Image, got {:?}",
                cut_slide.elements[0]
            );
        };
        assert!(
            *x_emu + *width_emu <= pt_to_emu(27.5),
            "clipped letter must not extend past the x=26 clip edge, got right={}",
            (*x_emu + *width_emu) as f64 / 12700.0
        );
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        let (w, h) = rgba.dimensions();
        assert!(
            rgba.get_pixel(w / 2, h / 2).0[3] > 200,
            "visible clipped left half must remain painted"
        );
        assert_eq!(
            rgba.get_pixel(w - 1, h / 2).0[3],
            0,
            "padding past the x=26 clip edge must remain transparent"
        );

        // 3. Enclosing page-wide clip [0..160]x[0..100] does not constrain the letter -> stays editable TextBox.
        let (extracted, full_slide) =
            run_case(b"q 0 0 160 100 re W n 0 0 0 rg BT /F1 40 Tf 10 30 Td (A) Tj ET Q");
        assert_eq!(extracted, "A");
        assert_eq!(full_slide.elements.len(), 1);
        assert!(
            matches!(&full_slide.elements[0], SlideElement::TextBox { text, .. } if text == "A"),
            "enclosing page clip must keep mapped Type 3 letter editable, got {:?}",
            full_slide.elements
        );

        // 4. Non-rectangular diagonal clip cutting across the letter -> rasterized and clipped.
        let (extracted, diag_slide) =
            run_case(b"q 10 30 m 42 30 l 10 62 l h W n 0 0 0 rg BT /F1 40 Tf 10 30 Td (A) Tj ET Q");
        assert_eq!(extracted, "A");
        assert_eq!(diag_slide.elements.len(), 1);
        let SlideElement::Image { image_data, .. } = &diag_slide.elements[0] else {
            panic!(
                "diagonal clip must flatten letter to clipped Image, got {:?}",
                diag_slide.elements[0]
            );
        };
        let diag_rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        let (dw, dh) = diag_rgba.dimensions();
        // Bottom-left inside triangle is opaque; top-right outside diagonal is transparent.
        assert!(
            diag_rgba.get_pixel(dw / 4, dh * 3 / 4).0[3] > 200,
            "inside of diagonal clip must remain opaque"
        );
        assert_eq!(
            diag_rgba.get_pixel(dw * 3 / 4, dh / 4).0[3],
            0,
            "outside of diagonal clip must be transparent"
        );
    }

    #[test]
    fn knockout_transparency_form_with_translucent_type3_glyph_matches_cpu_renderer() {
        use zpdf_render::{PageRenderInfo, RenderBackend};

        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Isolated knockout Form (/I true /K true) with Normal blend, alpha 1, no mask:
        // opaque red rectangle [10..90]x[10..90], then 50%-opaque blue Type 3 glyph (A)
        // overlapping at [30..70]x[30..70].
        let form = stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 100 100] \
             /Group << /Type /Group /S /Transparency /CS /DeviceRGB /I true /K true >> \
             /Resources << /Font << /F1 5 0 R >> /ExtGState << /GS50 10 0 R >> >>",
            b"1 0 0 rg 10 10 80 80 re f \
              /GS50 gs 0 0 1 rg BT /F1 50 Tf 30 30 Td (A) Tj ET",
        );
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R \
              /Resources << /XObject << /Fm 9 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"/Fm Do"),                         // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 65 /Widths [1000] /Resources << >> >>"
                .to_vec(), // 5
            b"<< /A 8 0 R >>".to_vec(),                    // 6
            b"<< /Type /Encoding /Differences [65 /A] >>".to_vec(), // 7
            stream("", b"1000 0 0 0 1000 1000 d1 0 0 800 800 re f"), // 8
            form,                                          // 9
            b"<< /Type /ExtGState /ca 0.5 >>".to_vec(),    // 10
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content_bytes);

        // Reference render via CpuRenderer on white background.
        let mut cpu = zpdf_render_cpu::CpuRenderer::new()
            .with_fonts(&fonts)
            .with_images(&images);
        cpu.begin_page(&PageRenderInfo {
            page_rect: dl.page_rect,
            scale: 2.0,
            background: Color::white(),
        })
        .expect("begin_page");
        for cmd in &dl.commands {
            cpu.execute(cmd).expect("execute");
        }
        let cpu_page = cpu.end_page().expect("end_page");
        // Center of overlap at PDF (50, 50) -> device (100, 100) on 200x200.
        let cpu_off = (100 * cpu_page.width as usize + 100) * 4;
        let cpu_overlap = [
            cpu_page.data[cpu_off],
            cpu_page.data[cpu_off + 1],
            cpu_page.data[cpu_off + 2],
        ];

        // PPTX export must composite the knockout Form as a single Image.
        let slide = display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
            .expect("slide export");
        assert_eq!(
            slide.elements.len(),
            1,
            "knockout Form must composite as one SlideElement::Image, got {:?}",
            slide.elements
        );
        let SlideElement::Image {
            x_emu,
            y_emu,
            width_emu,
            height_emu,
            image_data,
            ..
        } = &slide.elements[0]
        else {
            panic!("expected SlideElement::Image, got {:?}", slide.elements[0]);
        };
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        let (w, h) = rgba.dimensions();
        let crop_x0 = *x_emu as f64 / 12700.0;
        let crop_y1 = 100.0 - (*y_emu as f64 / 12700.0);
        let crop_w = *width_emu as f64 / 12700.0;
        let crop_h = *height_emu as f64 / 12700.0;
        let px_x = (((50.0 - crop_x0) / crop_w) * w as f64) as u32;
        let px_y = (((crop_y1 - 50.0) / crop_h) * h as f64) as u32;
        let overlap_rgba = rgba.get_pixel(px_x.min(w - 1), px_y.min(h - 1)).0;
        // Composite the straight-RGBA slide image over white to compare with CpuRenderer.
        let a = overlap_rgba[3] as f64 / 255.0;
        let slide_over_white = [
            (overlap_rgba[0] as f64 * a + 255.0 * (1.0 - a)).round() as u8,
            (overlap_rgba[1] as f64 * a + 255.0 * (1.0 - a)).round() as u8,
            (overlap_rgba[2] as f64 * a + 255.0 * (1.0 - a)).round() as u8,
        ];
        // Knockout clears red beneath the 50% blue glyph -> pure light blue (~128,128,255), NOT purple (~128,0,128).
        assert!(
            slide_over_white[1] > 115 && slide_over_white[2] > 240,
            "knockout overlap over white must be light blue (red knocked out), got png={overlap_rgba:?}, over_white={slide_over_white:?}, cpu={cpu_overlap:?}"
        );
        for c in 0..3 {
            assert!(
                slide_over_white[c].abs_diff(cpu_overlap[c]) <= 2,
                "knockout overlap channel {c} mismatch: slide={slide_over_white:?} vs cpu={cpu_overlap:?}"
            );
        }
    }

    #[test]
    fn translucent_native_rectangle_matches_overlapping_white_multiply_glyph() {
        let page_rect = Rect::new(0.0, 0.0, 200.0, 100.0);
        let mut dl = DisplayList::new(page_rect);
        // 50%-opaque blue rectangle across [20..180]x[20..80].
        let mut blue_rect = zpdf_display_list::Path::new();
        blue_rect.rect(Rect::new(20.0, 20.0, 180.0, 80.0));
        dl.push(RenderCommand::FillPath {
            path: blue_rect,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 0.5,
            overprint: None,
        });
        // White Multiply group covering right half [100..180]x[20..80].
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: page_rect,
            alpha: 1.0,
            mask: None,
        });
        let mut white_half = zpdf_display_list::Path::new();
        white_half.rect(Rect::new(100.0, 20.0, 180.0, 80.0));
        dl.push(RenderCommand::FillPath {
            path: white_half,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);

        let slide = display_list_to_slide(
            &dl,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");

        assert_eq!(
            slide.elements.len(),
            2,
            "expected translucent Rectangle + Multiply Image"
        );
        let (rect_rgb, rect_alpha) = match &slide.elements[0] {
            SlideElement::Opacity { element, alpha } => match element.as_ref() {
                SlideElement::Rectangle {
                    fill_rgb: Some(rgb),
                    ..
                } => (*rgb, *alpha),
                other => panic!("expected Rectangle inside Opacity, got {other:?}"),
            },
            other => panic!("expected SlideElement::Opacity(Rectangle), got {other:?}"),
        };
        assert!((rect_alpha - 0.5).abs() < 1e-3);
        let SlideElement::Image { image_data, .. } = &slide.elements[1] else {
            panic!(
                "expected Multiply SlideElement::Image, got {:?}",
                slide.elements[1]
            );
        };
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        let (w, h) = rgba.dimensions();
        let overlap = rgba.get_pixel(w / 2, h / 2).0;
        assert_eq!(overlap[3], 255);

        // Expected visual color of the native editable rectangle over white slide background.
        let expected_native = [
            (rect_rgb.0 as f32 * rect_alpha + 255.0 * (1.0 - rect_alpha)).round() as u8,
            (rect_rgb.1 as f32 * rect_alpha + 255.0 * (1.0 - rect_alpha)).round() as u8,
            (rect_rgb.2 as f32 * rect_alpha + 255.0 * (1.0 - rect_alpha)).round() as u8,
        ];
        for c in 0..3 {
            assert!(
                overlap[c].abs_diff(expected_native[c]) <= 1,
                "white Multiply overlap {overlap:?} must match adjacent translucent editable shape over white {expected_native:?}"
            );
        }

        // Verify OOXML preserves the 50% fill alpha on the native Rectangle.
        let bytes = PptxWriter::new(PptxPresentation {
            slides: vec![slide],
            width_emu: pt_to_emu(200.0),
            height_emu: pt_to_emu(100.0),
        })
        .write_to_bytes()
        .expect("write pptx");
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
        let xml = String::from_utf8(zip_entry(&mut archive, "ppt/slides/slide1.xml")).unwrap();
        assert!(
            xml.contains(r#"<a:srgbClr val="0000FF"><a:alpha val="50000"/></a:srgbClr>"#),
            "native translucent rectangle XML must carry matching 50% alpha: {xml}"
        );
    }

    #[test]
    fn disjoint_multiply_groups_scale_linearly_in_command_visits() {
        let build_dl = |count: usize| -> DisplayList {
            let page_rect = Rect::new(0.0, 0.0, 400.0, 400.0);
            let mut dl = DisplayList::new(page_rect);
            let mut bg = zpdf_display_list::Path::new();
            bg.rect(page_rect);
            dl.push(RenderCommand::FillPath {
                path: bg,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
                alpha: 1.0,
                overprint: None,
            });
            for i in 0..count {
                let col = (i % 20) as f64;
                let row = (i / 20) as f64;
                let x0 = 5.0 + col * 18.0;
                let y0 = 5.0 + row * 18.0;
                let r = Rect::new(x0, y0, x0 + 12.0, y0 + 12.0);
                dl.push(RenderCommand::PushBlendGroup {
                    blend_mode: zpdf_display_list::BlendMode::Multiply,
                    isolated: true,
                    knockout: false,
                    bounds: r,
                    alpha: 1.0,
                    mask: None,
                });
                let mut p = zpdf_display_list::Path::new();
                p.rect(r);
                dl.push(RenderCommand::FillPath {
                    path: p,
                    rule: zpdf_display_list::FillRule::NonZero,
                    paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
                    alpha: 1.0,
                    overprint: None,
                });
                dl.push(RenderCommand::PopBlendGroup);
            }
            dl
        };

        let dl_small = build_dl(20);
        let (slide_small, stats_small) = display_list_to_slide_internal(
            &dl_small,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
            Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
            None,
        )
        .expect("small export");

        let dl_large = build_dl(200);
        let (slide_large, stats_large) = display_list_to_slide_internal(
            &dl_large,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
            Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
            None,
        )
        .expect("large export");

        assert_eq!(slide_small.elements.len(), 21);
        assert_eq!(slide_large.elements.len(), 201);

        // Backdrop must execute each command at most once across the entire page.
        assert_eq!(
            stats_small.backdrop_command_visits,
            dl_small.commands.len(),
            "backdrop must visit each command once for N=20, got {stats_small:?}"
        );
        assert_eq!(
            stats_large.backdrop_command_visits,
            dl_large.commands.len(),
            "backdrop must visit each command once for N=200, got {stats_large:?}"
        );

        let total_small = stats_small.backdrop_command_visits + stats_small.pass1_command_visits;
        let total_large = stats_large.backdrop_command_visits + stats_large.pass1_command_visits;
        // 10x more groups -> strictly linear (~10x visits, far below quadratic ~100x).
        assert!(
            total_large <= total_small * 12,
            "command visits must scale linearly with group count: N=20 had {total_small}, N=200 had {total_large}"
        );

        // Verify first and last Multiply tiles both blended red*blue = black.
        for idx in [1, 200] {
            let SlideElement::Image { image_data, .. } = &slide_large.elements[idx] else {
                panic!("expected SlideElement::Image at {idx}");
            };
            let rgba = image::load_from_memory(image_data)
                .expect("decode PNG")
                .to_rgba8();
            let (w, h) = rgba.dimensions();
            assert_eq!(
                rgba.get_pixel(w / 2, h / 2).0,
                [0, 0, 0, 255],
                "Multiply tile {idx} over blue backdrop must be black"
            );
        }
    }

    #[test]
    fn overlapping_multiply_groups_composite_incrementally() {
        let page_rect = Rect::new(0.0, 0.0, 140.0, 140.0);
        let mut dl = DisplayList::new(page_rect);
        // Group 1: cyan [20..100]x[20..100] Multiply over white -> cyan (0, 255, 255).
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: page_rect,
            alpha: 1.0,
            mask: None,
        });
        let mut cyan = zpdf_display_list::Path::new();
        cyan.rect(Rect::new(20.0, 20.0, 100.0, 100.0));
        dl.push(RenderCommand::FillPath {
            path: cyan,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 1.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);

        // Group 2: yellow [60..120]x[20..100] Multiply overlapping cyan on [60..100] -> green (0, 255, 0),
        // and over white on [100..120] -> yellow (255, 255, 0).
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: page_rect,
            alpha: 1.0,
            mask: None,
        });
        let mut yellow = zpdf_display_list::Path::new();
        yellow.rect(Rect::new(60.0, 20.0, 120.0, 100.0));
        dl.push(RenderCommand::FillPath {
            path: yellow,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 1.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);

        let (slide, stats) = display_list_to_slide_internal(
            &dl,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
            Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
            None,
        )
        .expect("slide export");

        assert_eq!(stats.backdrop_command_visits, dl.commands.len());
        assert_eq!(slide.elements.len(), 2);
        let SlideElement::Image { image_data, .. } = &slide.elements[1] else {
            panic!("expected second Multiply SlideElement::Image");
        };
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        let (w, h) = rgba.dimensions();
        // Left part of yellow [60..100] overlaps cyan -> green (0, 255, 0, 255).
        assert_eq!(
            rgba.get_pixel(w / 4, h / 2).0,
            [0, 255, 0, 255],
            "overlapping yellow Multiply over prior cyan Multiply must yield green"
        );
        // Right part of yellow [100..120] is over white -> yellow (255, 255, 0, 255).
        assert_eq!(
            rgba.get_pixel(w * 5 / 6, h / 2).0,
            [255, 255, 0, 255],
            "non-overlapping yellow Multiply over white must stay yellow"
        );
    }

    #[test]
    fn expired_backdrop_deadline_returns_error_instead_of_copying_stale_pixels() {
        let page_rect = Rect::new(0.0, 0.0, 200.0, 100.0);
        let mut dl = DisplayList::new(page_rect);
        let mut bg = zpdf_display_list::Path::new();
        bg.rect(page_rect);
        dl.push(RenderCommand::FillPath {
            path: bg,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        for x0 in [10.0, 110.0] {
            let r = Rect::new(x0, 20.0, x0 + 60.0, 80.0);
            dl.push(RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Multiply,
                isolated: true,
                knockout: false,
                bounds: r,
                alpha: 1.0,
                mask: None,
            });
            let mut p = zpdf_display_list::Path::new();
            p.rect(r);
            dl.push(RenderCommand::FillPath {
                path: p,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
                alpha: 1.0,
                overprint: None,
            });
            dl.push(RenderCommand::PopBlendGroup);
        }

        // Case 1: already-expired budget at start must fail cleanly.
        let err = display_list_to_slide_internal(
            &dl,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
            Some(std::time::Duration::ZERO),
            None,
        )
        .err()
        .expect("expired render budget must return an export error");
        assert!(
            err.to_string().contains("time budget"),
            "unexpected error message: {err}"
        );

        // Case 2: first blend completes, then the backdrop deadline expires before
        // the second blend — must surface an error rather than stamping stale pixels.
        let err2 = display_list_to_slide_internal(
            &dl,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
            Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
            Some(1),
        )
        .err()
        .expect("mid-page backdrop deadline expiry must return an export error");
        assert!(
            err2.to_string().contains("time budget"),
            "unexpected mid-page error message: {err2}"
        );
    }

    #[test]
    fn small_multiply_glyph_on_large_sheet_renders_at_crop_scale_with_bounded_memory_and_linear_visits(
    ) {
        let page_rect = Rect::new(0.0, 0.0, 8192.0, 8192.0);
        let mut dl = DisplayList::new(page_rect);
        // Light cyan backdrop across the entire 8192x8192 sheet.
        let mut bg = zpdf_display_list::Path::new();
        bg.rect(page_rect);
        dl.push(RenderCommand::FillPath {
            path: bg,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 1.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        // Small 12x12pt Multiply glyph at (100, 100) with two separated 2pt vertical strokes
        // ([100..102] and [110..112]) and an 8pt transparent gap ([102..110]) between them.
        let glyph_bounds = Rect::new(100.0, 100.0, 112.0, 112.0);
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: glyph_bounds,
            alpha: 1.0,
            mask: None,
        });
        let mut left_stem = zpdf_display_list::Path::new();
        left_stem.rect(Rect::new(100.0, 100.0, 102.0, 112.0));
        dl.push(RenderCommand::FillPath {
            path: left_stem,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 1.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        let mut right_stem = zpdf_display_list::Path::new();
        right_stem.rect(Rect::new(110.0, 100.0, 112.0, 112.0));
        dl.push(RenderCommand::FillPath {
            path: right_stem,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 1.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);

        let (slide, stats) = display_list_to_slide_internal(
            &dl,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
            Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
            None,
        )
        .expect("large-sheet small glyph export");
        assert!(
            stats.peak_backdrop_pixels <= MAX_BACKDROP_CACHE_PIXELS,
            "backdrop allocation must stay within 16 MiB pixel cap, got {}",
            stats.peak_backdrop_pixels
        );
        assert_eq!(slide.elements.len(), 2);
        let SlideElement::Image { image_data, .. } = &slide.elements[1] else {
            panic!("expected Multiply SlideElement::Image");
        };
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        let (w, h) = rgba.dimensions();
        // At crop scale 2.0, the padded 14x14pt crop renders at ~28x28px (NOT 3x3px at page scale 0.25!).
        assert!(
            w >= 24 && h >= 24,
            "small 12pt Multiply glyph on 8192pt sheet must render at crop scale (~28x28px), got {w}x{h}"
        );
        // Left stem and right stem must be green (Multiply(yellow, cyan)) while the center gap stays transparent.
        let mid_y = h / 2;
        let left_px = rgba.get_pixel(3, mid_y).0;
        let gap_px = rgba.get_pixel(w / 2, mid_y).0;
        let right_px = rgba.get_pixel(w - 4, mid_y).0;
        assert_eq!(
            left_px,
            [0, 255, 0, 255],
            "left stem must be green, got {left_px:?}"
        );
        assert_eq!(
            gap_px[3], 0,
            "gap between stems must remain transparent, got {gap_px:?}"
        );
        assert_eq!(
            right_px,
            [0, 255, 0, 255],
            "right stem must be green, got {right_px:?}"
        );

        // Also check bounded memory + linear command visits across many disjoint tiles on the 8192x8192 sheet.
        let build_many = |count: usize| -> DisplayList {
            let mut d = DisplayList::new(page_rect);
            let mut full = zpdf_display_list::Path::new();
            full.rect(page_rect);
            d.push(RenderCommand::FillPath {
                path: full,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
                alpha: 1.0,
                overprint: None,
            });
            for i in 0..count {
                // Place groups across 8 distinct 512x512 tiles along the sheet.
                let tile = (i % 8) as f64;
                let slot = (i / 8) as f64;
                let x0 = tile * 600.0 + 20.0 + (slot % 10.0) * 20.0;
                let y0 = 20.0 + (slot / 10.0).floor() * 20.0;
                let r = Rect::new(x0, y0, x0 + 12.0, y0 + 12.0);
                d.push(RenderCommand::PushBlendGroup {
                    blend_mode: zpdf_display_list::BlendMode::Multiply,
                    isolated: true,
                    knockout: false,
                    bounds: r,
                    alpha: 1.0,
                    mask: None,
                });
                let mut p = zpdf_display_list::Path::new();
                p.rect(r);
                d.push(RenderCommand::FillPath {
                    path: p,
                    rule: zpdf_display_list::FillRule::NonZero,
                    paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
                    alpha: 1.0,
                    overprint: None,
                });
                d.push(RenderCommand::PopBlendGroup);
            }
            d
        };
        let dl_many = build_many(80);
        let (slide_many, stats_many) = display_list_to_slide_internal(
            &dl_many,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
            Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
            None,
        )
        .expect("many small glyphs on large sheet");
        assert_eq!(slide_many.elements.len(), 81);
        assert!(
            stats_many.peak_backdrop_pixels <= MAX_BACKDROP_CACHE_PIXELS,
            "multi-tile backdrop cache must stay bounded <= 16 MiB pixels, got {}",
            stats_many.peak_backdrop_pixels
        );
        assert!(
            stats_many.backdrop_command_visits <= dl_many.commands.len() * 6,
            "tiled spatial index must keep backdrop command visits linear (not O(N^2)), got {} for {} commands",
            stats_many.backdrop_command_visits,
            dl_many.commands.len()
        );
    }

    #[test]
    fn backdrop_rebaser_bounds_retained_masks_and_keeps_cpu_mask_cache_consistent() {
        let page_rect = Rect::new(0.0, 0.0, 200.0, 200.0);

        // Part 1: Pattern tiles whose shared mask geometry lies inside page_rect
        // must reuse CpuRenderer's native offset plane with ZERO retained translations.
        let mut in_bounds_mask_dl = DisplayList::new(page_rect);
        let mut mrect = zpdf_display_list::Path::new();
        mrect.rect(Rect::new(0.0, 0.0, 10.0, 10.0));
        in_bounds_mask_dl.push(RenderCommand::FillPath {
            path: mrect,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let shared_in_bounds = std::sync::Arc::new(in_bounds_mask_dl);
        let mut dl_in = DisplayList::new(page_rect);
        for i in 0..150 {
            let x = (i % 15) as f32 * 12.0;
            let y = (i / 15) as f32 * 12.0;
            dl_in.push(RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: Rect::new(x as f64, y as f64, x as f64 + 10.0, y as f64 + 10.0),
                alpha: 1.0,
                mask: Some(zpdf_display_list::SoftMask {
                    kind: zpdf_display_list::SoftMaskKind::Alpha,
                    commands: std::sync::Arc::clone(&shared_in_bounds),
                    offset: (x, y),
                    backdrop_luma: 0.0,
                    transfer: None,
                }),
            });
            let mut tile = zpdf_display_list::Path::new();
            tile.rect(Rect::new(
                x as f64,
                y as f64,
                x as f64 + 10.0,
                y as f64 + 10.0,
            ));
            dl_in.push(RenderCommand::FillPath {
                path: tile,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
                alpha: 1.0,
                overprint: None,
            });
            dl_in.push(RenderCommand::PopBlendGroup);
        }
        // Trigger backdrop replay with a trailing Multiply group.
        dl_in.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 20.0, 20.0),
            alpha: 1.0,
            mask: None,
        });
        let mut probe = zpdf_display_list::Path::new();
        probe.rect(Rect::new(0.0, 0.0, 10.0, 10.0));
        dl_in.push(RenderCommand::FillPath {
            path: probe,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl_in.push(RenderCommand::PopBlendGroup);

        let (_slide_in, stats_in) = display_list_to_slide_internal(
            &dl_in,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
            Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
            None,
        )
        .expect("in-bounds pattern export");
        assert_eq!(
            stats_in.peak_rebaser_mask_entries, 0,
            "in-bounds shared pattern masks must not be duplicated in SoftMaskRebaser"
        );

        // Part 2: Off-page shared mask translated to > MAX_TRANSLATED_MASK_ENTRIES
        // distinct offsets must remain strictly bounded in retained cache size AND
        // produce accurate uncorrupted mask output past the cap.
        let mut off_page_mask_dl = DisplayList::new(Rect::new(-100.0, -100.0, 0.0, 0.0));
        let mut off_rect = zpdf_display_list::Path::new();
        off_rect.rect(Rect::new(-50.0, -50.0, -40.0, -40.0));
        off_page_mask_dl.push(RenderCommand::FillPath {
            path: off_rect,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let shared_off_page = std::sync::Arc::new(off_page_mask_dl);

        let mut dl_off = DisplayList::new(page_rect);
        // 120 distinct offsets (> MAX_TRANSLATED_MASK_ENTRIES = 64); the last tile lands at [150..160]x[150..160].
        for i in 0..120 {
            let target_x = if i == 119 {
                150.0f32
            } else {
                (i % 12) as f32 * 10.0
            };
            let target_y = if i == 119 {
                150.0f32
            } else {
                (i / 12) as f32 * 10.0
            };
            let off_x = target_x + 50.0;
            let off_y = target_y + 50.0;
            dl_off.push(RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: page_rect,
                alpha: 1.0,
                mask: Some(zpdf_display_list::SoftMask {
                    kind: zpdf_display_list::SoftMaskKind::Alpha,
                    commands: std::sync::Arc::clone(&shared_off_page),
                    offset: (off_x, off_y),
                    backdrop_luma: 0.0,
                    transfer: None,
                }),
            });
            let mut tile = zpdf_display_list::Path::new();
            tile.rect(Rect::new(
                target_x as f64,
                target_y as f64,
                target_x as f64 + 10.0,
                target_y as f64 + 10.0,
            ));
            dl_off.push(RenderCommand::FillPath {
                path: tile,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
                alpha: 1.0,
                overprint: None,
            });
            dl_off.push(RenderCommand::PopBlendGroup);
        }
        // Multiply over the final post-cap tile at [150..160]x[150..160] and an unpainted neighbor [170..180].
        dl_off.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: Rect::new(150.0, 150.0, 180.0, 160.0),
            alpha: 1.0,
            mask: None,
        });
        let mut probe_rect = zpdf_display_list::Path::new();
        probe_rect.rect(Rect::new(150.0, 150.0, 180.0, 160.0));
        dl_off.push(RenderCommand::FillPath {
            path: probe_rect,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl_off.push(RenderCommand::PopBlendGroup);

        let (slide_off, stats_off) = display_list_to_slide_internal(
            &dl_off,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
            Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
            None,
        )
        .expect("off-page bounded rebaser export");
        assert!(
            stats_off.peak_rebaser_mask_entries <= MAX_TRANSLATED_MASK_ENTRIES,
            "retained translated masks must stay <= {MAX_TRANSLATED_MASK_ENTRIES}, got {}",
            stats_off.peak_rebaser_mask_entries
        );
        assert!(
            stats_off.peak_rebaser_mask_bytes <= MAX_TRANSLATED_MASK_BYTES,
            "retained translated mask bytes must stay <= {MAX_TRANSLATED_MASK_BYTES}, got {}",
            stats_off.peak_rebaser_mask_bytes
        );
        let SlideElement::Image { image_data, .. } =
            slide_off.elements.last().expect("trailing Multiply image")
        else {
            panic!("expected trailing Multiply SlideElement::Image");
        };
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        let (w, h) = rgba.dimensions();
        // Left third [150..160] overlaps the 120th post-cap blue tile -> black.
        assert_eq!(
            rgba.get_pixel(w / 6, h / 2).0,
            [0, 0, 0, 255],
            "post-cap translated mask tile must render accurately onto backdrop without stale cache collision"
        );
        // Right third [170..180] is over white -> red.
        assert_eq!(
            rgba.get_pixel(w * 5 / 6, h / 2).0,
            [255, 0, 0, 255],
            "unpainted neighbor outside post-cap mask tile must remain white->red"
        );
    }

    #[test]
    fn sheared_type3_inline_bitmap_preserves_parallelogram_geometry() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Unmapped Type 3 glyph draws a solid 2x2 red RGB inline bitmap over [0..1000]^2.
        // Shown with sheared text matrix `1 0 1 1 20 20 Tm` at 40pt:
        // maps unit square [0..1]^2 -> parallelogram with corners (20,20), (60,20), (60,60), (100,60),
        // bounding box [20..100] x [20..60] (width = 80pt, height = 40pt).
        let mut glyph_ops =
            b"1000 0 d0 1000 0 0 1000 0 0 cm BI /W 2 /H 2 /BPC 8 /CS /RGB ID ".to_vec();
        glyph_ops.extend_from_slice(&[255, 0, 0, 255, 0, 0, 255, 0, 0, 255, 0, 0]);
        glyph_ops.extend_from_slice(b"\nEI");

        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 140 100] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"BT /F1 40 Tf 1 0 1 1 20 20 Tm (A) Tj ET"), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 65 /Widths [1000] /Resources << >> >>"
                .to_vec(), // 5
            b"<< /logo 8 0 R >>".to_vec(),                 // 6
            b"<< /Type /Encoding /Differences [65 /logo] >>".to_vec(), // 7
            stream("", &glyph_ops),                        // 8
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
            .expect("slide export");
        assert_eq!(slide.elements.len(), 1);
        // Must be flattened to an untransformed cropped SlideElement::Image covering [20..100]x[20..60].
        let SlideElement::Image {
            width_emu,
            height_emu,
            image_data,
            ..
        } = &slide.elements[0]
        else {
            panic!(
                "sheared inline bitmap must flatten to affine-rasterized SlideElement::Image, got {:?}",
                slide.elements[0]
            );
        };
        assert!(
            *width_emu >= pt_to_emu(79.0) && *height_emu >= pt_to_emu(39.0),
            "sheared parallelogram bounding box must cover 80x40pt, got {}x{}",
            *width_emu as f64 / 12700.0,
            *height_emu as f64 / 12700.0
        );
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        let (w, h) = rgba.dimensions();
        // Center of parallelogram is opaque red; top-left and bottom-right bounding-box corners
        // lie outside the parallelogram and must remain transparent!
        let center = rgba.get_pixel(w / 2, h / 2).0;
        assert!(
            center[3] > 240 && center[0] > 240 && center[1] < 15 && center[2] < 15,
            "sheared bitmap interior must be opaque red, got {center:?}"
        );
        assert_eq!(
            rgba.get_pixel(4, 4).0[3],
            0,
            "top-left corner outside sheared parallelogram must stay transparent"
        );
        assert_eq!(
            rgba.get_pixel(w - 5, h - 5).0[3],
            0,
            "bottom-right corner outside sheared parallelogram must stay transparent"
        );
    }

    #[test]
    fn inconsistent_inline_image_dimensions_return_error_without_panicking() {
        let cases = [
            (0, 2, vec![]),
            (2, 0, vec![]),
            (2, 2, vec![255, 0, 0, 255]),     // truncated (4 vs 16)
            (1, 1, vec![255; 8]),             // oversized (8 vs 4)
            (u32::MAX, u32::MAX, vec![0; 4]), // checked_mul overflow
        ];
        for (width, height, data) in cases {
            for transform in [
                Matrix::new(20.0, 0.0, 0.0, 20.0, 10.0, 10.0),
                Matrix::new(20.0, 0.0, 10.0, 20.0, 10.0, 10.0), // sheared path too
            ] {
                let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 80.0, 80.0));
                dl.push(RenderCommand::DrawInlineImage(
                    zpdf_display_list::InlineImageDraw {
                        image: std::sync::Arc::new(zpdf_display_list::InlineImage {
                            width,
                            height,
                            data: data.clone(),
                            has_alpha: false,
                            is_image_mask: false,
                            premultiplied: false,
                        }),
                        transform,
                        alpha: 1.0,
                    },
                ));
                let res = display_list_to_slide(
                    &dl,
                    0,
                    &FontCache::new(),
                    &ImageCache::new(),
                    &ConversionOptions::default(),
                );
                assert!(
                    res.is_err(),
                    "inconsistent inline image ({width}x{height}, {} bytes) must return Err",
                    data.len()
                );
            }
        }
    }

    #[test]
    fn clipped_type3_glyph_with_leading_local_clip_keeps_paint_after_pop_clip() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // CharProc pushes a local clip [0..200], draws a red rect inside [0..200],
        // pops the local clip, then draws a blue rect at [500..900] OUTSIDE the local clip!
        // Page installs a caller clip [0..60] cutting across [0..90] (so both red at [10..26]
        // and part of blue at [50..60] are inside the caller clip).
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 120 100] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"q 0 0 60 100 re W n BT /F1 80 Tf 10 10 Td (A) Tj ET Q"), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 65 /Widths [1000] /Resources << >> >>"
                .to_vec(), // 5
            b"<< /logo 8 0 R >>".to_vec(),                 // 6
            b"<< /Type /Encoding /Differences [65 /logo] >>".to_vec(), // 7
            stream(
                "",
                b"1000 0 d0 q 0 0 200 1000 re W n 1 0 0 rg 0 0 200 800 re f Q \
                  0 0 1 rg 500 0 400 800 re f",
            ), // 8
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
            .expect("slide export");
        assert_eq!(slide.elements.len(), 1);
        let SlideElement::Image { image_data, .. } = &slide.elements[0] else {
            panic!("expected SlideElement::Image, got {:?}", slide.elements[0]);
        };
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        let has_red = rgba
            .pixels()
            .any(|p| p.0[3] > 200 && p.0[0] > 200 && p.0[2] < 40);
        let has_blue = rgba
            .pixels()
            .any(|p| p.0[3] > 200 && p.0[2] > 200 && p.0[0] < 40);
        assert!(has_red, "red fill inside leading local clip must survive");
        assert!(
            has_blue,
            "blue fill AFTER PopClip must NOT be cropped away by the leading local clip"
        );
    }

    #[test]
    fn type3_opaque_dashed_stroke_preserves_dash_gaps_in_pptx() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // /dash_icon draws an opaque dashed line (`[200 200] 0 d`);
        // /solid_icon draws an opaque solid line (stays native editable Line).
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 100] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"0 0 1 RG BT /F1 100 Tf 10 40 Td (AB) Tj ET"), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 66 /Widths [1000 1000] /Resources << >> >>"
                .to_vec(), // 5
            b"<< /dash_icon 8 0 R /solid_icon 9 0 R >>".to_vec(), // 6
            b"<< /Type /Encoding /Differences [65 /dash_icon /solid_icon] >>".to_vec(), // 7
            stream("", b"1000 0 d0 80 w [200 200] 0 d 0 100 m 800 100 l S"), // 8
            stream("", b"1000 0 d0 80 w 0 100 m 800 100 l S"), // 9
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
            .expect("slide export");
        assert_eq!(slide.elements.len(), 2);
        let SlideElement::Image { image_data, .. } = &slide.elements[0] else {
            panic!(
                "dashed Type 3 artwork stroke must rasterize to preserve dash gaps, got {:?}",
                slide.elements[0]
            );
        };
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        let (w, h) = rgba.dimensions();
        let mid_y = h / 2;
        let on_px = (0..w)
            .filter(|&x| rgba.get_pixel(x, mid_y).0[3] > 200)
            .count();
        let off_px = (0..w)
            .filter(|&x| rgba.get_pixel(x, mid_y).0[3] == 0)
            .count();
        assert!(
            on_px > 10 && off_px > 10,
            "dashed stroke centerline must contain both opaque dashes ({on_px}) and transparent gaps ({off_px})"
        );
        assert!(
            matches!(slide.elements[1], SlideElement::Line { .. }),
            "solid Type 3 stroke must stay native editable Line, got {:?}",
            slide.elements[1]
        );
    }

    #[test]
    fn type3_word_overlapped_by_multiply_blend_keeps_consistent_artwork_while_unoverlapped_stays_editable(
    ) {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Light gray page backdrop, blue Type 3 letter (A) at [20..44]x[40..64],
        // blue Type 3 letter (B) at [120..144]x[40..64], then:
        // 1. a white Multiply group covering the right half of (A) ([32..56]x[40..64]), and
        // 2. a page-wide Multiply fill ([0..200]x[0..100]) clipped by the caller to the
        //    disjoint right margin [160..195]x[0..100] (must NOT cost (B) its TextBox).
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> /ExtGState << /GSMul 9 0 R >> >> >>"
                .to_vec(), // 3
            stream(
                "",
                b"0.9 0.9 0.9 rg 5 5 190 90 re f \
                  0 0 1 rg BT /F1 24 Tf 20 40 Td (A) Tj ET \
                  0 0 1 rg BT /F1 24 Tf 120 40 Td (B) Tj ET \
                  q /GSMul gs 1 1 1 rg 32 40 24 24 re f Q \
                  q 160 0 35 100 re W n /GSMul gs 1 0 0 rg 0 0 200 100 re f Q",
            ), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 66 /Widths [1000 1000] /Resources << >> >>"
                .to_vec(), // 5
            b"<< /A 8 0 R /B 8 0 R >>".to_vec(),           // 6
            b"<< /Type /Encoding /Differences [65 /A /B] >>".to_vec(), // 7
            stream("", b"1000 0 d0 0 0 800 800 re f"),     // 8
            b"<< /Type /ExtGState /BM /Multiply >>".to_vec(), // 9
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let mut spans = Vec::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .with_text_sink(&mut spans)
            .interpret_with_stats(&content_bytes);

        let extracted: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(extracted, "AB");

        let slide = display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
            .expect("slide export");

        // (A) is overlapped by the first Multiply group -> falls back to CharProc Rectangle
        // so both uncovered and Multiply-covered halves render identical blue artwork (no
        // half-TextBox / half-Rectangle seam). (B) is untouched by both the first Multiply
        // and the disjointly clipped page-wide Multiply -> stays editable TextBox("B").
        let texts: Vec<&str> = slide
            .elements
            .iter()
            .filter_map(|e| match e.base() {
                SlideElement::TextBox { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            vec!["B"],
            "overlapped Type 3 glyph (A) must use consistent artwork while untouched/clip-disjoint (B) stays editable TextBox: {:?}",
            slide.elements
        );
        assert!(
            slide.elements.iter().any(|e| matches!(
                e.base(),
                SlideElement::Rectangle {
                    fill_rgb: Some((0, 0, 255)),
                    ..
                }
            )),
            "overlapped Type 3 glyph (A) must export its blue CharProc rectangle: {:?}",
            slide.elements
        );
    }

    #[test]
    fn large_corner_multiply_panel_on_large_sheet_keeps_opposite_corner_type3_word_editable() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // 8192x8192 sheet: mapped Type 3 (A) in bottom-left [100..124], mapped Type 3 (B)
        // in top-right [5100..5124], and a 3000x3000pt Multiply panel [5000..8000]^2 that
        // spans 36 (> 16) spatial index cells in the top-right corner only.
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 8192 8192] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> /ExtGState << /GSMul 9 0 R >> >> >>"
                .to_vec(), // 3
            stream(
                "",
                b"0 0 1 rg BT /F1 24 Tf 100 100 Td (A) Tj ET \
                  0 0 1 rg BT /F1 24 Tf 5100 5100 Td (B) Tj ET \
                  q /GSMul gs 1 0 0 rg 5000 5000 3000 3000 re f Q",
            ), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 66 /Widths [1000 1000] /Resources << >> >>"
                .to_vec(), // 5
            b"<< /A 8 0 R /B 8 0 R >>".to_vec(),           // 6
            b"<< /Type /Encoding /Differences [65 /A /B] >>".to_vec(), // 7
            stream("", b"1000 0 d0 0 0 800 800 re f"),     // 8
            b"<< /Type /ExtGState /BM /Multiply >>".to_vec(), // 9
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let mut spans = Vec::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .with_text_sink(&mut spans)
            .interpret_with_stats(&content_bytes);

        let extracted: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(extracted, "AB");

        let slide = display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
            .expect("slide export");
        let texts: Vec<&str> = slide
            .elements
            .iter()
            .filter_map(|e| match e.base() {
                SlideElement::TextBox { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            vec!["A"],
            "opposite-corner Type 3 glyph (A) must stay editable TextBox despite >16-cell Multiply panel in top-right: {:?}",
            slide.elements
        );
        assert!(
            slide.elements.iter().any(|e| matches!(
                e.base(),
                SlideElement::Rectangle {
                    fill_rgb: Some((0, 0, 255)),
                    ..
                }
            )),
            "Type 3 glyph (B) inside the large corner Multiply panel must keep consistent CharProc artwork"
        );
    }

    #[test]
    fn five_region_cycle_tiled_multiply_groups_scale_linearly_without_eviction_replay() {
        let page_rect = Rect::new(0.0, 0.0, 8192.0, 8192.0);
        let tile_origins = [20.0, 600.0, 1200.0, 1800.0, 2600.0];
        let build_cycle = |rounds: usize| -> DisplayList {
            let mut dl = DisplayList::new(page_rect);
            let mut bg = zpdf_display_list::Path::new();
            bg.rect(page_rect);
            dl.push(RenderCommand::FillPath {
                path: bg,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
                alpha: 1.0,
                overprint: None,
            });
            for round in 0..rounds {
                for (region, &base_x) in tile_origins.iter().enumerate() {
                    let x0 = base_x + (round % 10) as f64 * 20.0;
                    let y0 = 20.0 + (round / 10) as f64 * 20.0;
                    let r = Rect::new(x0, y0, x0 + 12.0, y0 + 12.0);
                    let _ = region;
                    dl.push(RenderCommand::PushBlendGroup {
                        blend_mode: zpdf_display_list::BlendMode::Multiply,
                        isolated: true,
                        knockout: false,
                        bounds: r,
                        alpha: 1.0,
                        mask: None,
                    });
                    let mut p = zpdf_display_list::Path::new();
                    p.rect(r);
                    dl.push(RenderCommand::FillPath {
                        path: p,
                        rule: zpdf_display_list::FillRule::NonZero,
                        paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
                        alpha: 1.0,
                        overprint: None,
                    });
                    dl.push(RenderCommand::PopBlendGroup);
                }
            }
            dl
        };

        let dl_small = build_cycle(4); // 5 regions x 4 rounds = 20 groups
        let dl_large = build_cycle(40); // 5 regions x 40 rounds = 200 groups (10x)

        let (slide_small, stats_small) = display_list_to_slide_internal(
            &dl_small,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
            Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
            None,
        )
        .expect("small five-region cycle");
        let (slide_large, stats_large) = display_list_to_slide_internal(
            &dl_large,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
            Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
            None,
        )
        .expect("large five-region cycle");

        assert_eq!(slide_small.elements.len(), 21);
        assert_eq!(slide_large.elements.len(), 201);
        assert!(
            stats_small.peak_backdrop_pixels <= 1024 * 1024
                && stats_large.peak_backdrop_pixels <= 1024 * 1024,
            "deferred single-tile batching must hold at most one 512pt tile (1024x1024 px), got {} / {}",
            stats_small.peak_backdrop_pixels,
            stats_large.peak_backdrop_pixels
        );
        // Under eviction replay, 10x rounds would grow ~82x (sum 1..40 vs 1..4);
        // single-pass per-tile batching visits each group in its tile exactly once.
        assert!(
            stats_large.backdrop_command_visits <= stats_small.backdrop_command_visits * 11,
            "five-region cycle must scale linearly without eviction replay: small={} large={}",
            stats_small.backdrop_command_visits,
            stats_large.backdrop_command_visits
        );
        assert!(
            stats_large.backdrop_command_visits <= 5 + 200 * 3 + 20,
            "each tile must execute the background + its own groups once, got {}",
            stats_large.backdrop_command_visits
        );

        // Verify exact paint Z-order is preserved across deferred tile flushing.
        for idx in 0..20 {
            let expected_base = tile_origins[idx % 5];
            let SlideElement::Image {
                x_emu, image_data, ..
            } = &slide_small.elements[idx + 1]
            else {
                panic!("expected SlideElement::Image at element {}", idx + 1);
            };
            let x_pt = *x_emu as f64 / 12700.0;
            assert!(
                x_pt >= expected_base - 2.0 && x_pt <= expected_base + 220.0,
                "element {} must preserve five-region Z-order (expected region around {expected_base}, got x={x_pt})",
                idx + 1
            );
            if idx >= 15 {
                let rgba = image::load_from_memory(image_data)
                    .expect("decode PNG")
                    .to_rgba8();
                let (w, h) = rgba.dimensions();
                assert_eq!(
                    rgba.get_pixel(w / 2, h / 2).0,
                    [0, 0, 0, 255],
                    "red Multiply over blue backdrop must blend to black in region {}",
                    idx % 5
                );
            }
        }

        // Also cover shifted, overlapping fills wider than one 512pt tile (600pt and 1200pt)
        // at two sequence lengths (10 vs 80) so coalesced grid windows prevent quadratic replay.
        let build_shifted_wide = |count: usize, width: f64, step_x: f64| -> DisplayList {
            let mut dl = DisplayList::new(page_rect);
            let mut bg = zpdf_display_list::Path::new();
            bg.rect(page_rect);
            dl.push(RenderCommand::FillPath {
                path: bg,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
                alpha: 1.0,
                overprint: None,
            });
            for i in 0..count {
                let x0 = 20.0 + i as f64 * step_x;
                let r = Rect::new(x0, 20.0, x0 + width, 32.0);
                dl.push(RenderCommand::PushBlendGroup {
                    blend_mode: zpdf_display_list::BlendMode::Multiply,
                    isolated: true,
                    knockout: false,
                    bounds: r,
                    alpha: 1.0,
                    mask: None,
                });
                let mut p = zpdf_display_list::Path::new();
                p.rect(r);
                dl.push(RenderCommand::FillPath {
                    path: p,
                    rule: zpdf_display_list::FillRule::NonZero,
                    paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
                    alpha: 1.0,
                    overprint: None,
                });
                dl.push(RenderCommand::PopBlendGroup);
            }
            dl
        };

        for (width, step_x, min_px_w, label) in [
            (600.0, 4.0, 1100u32, "600pt shifted overlapping fills"),
            (1200.0, 8.0, 1100u32, "1200pt shifted overlapping fills"),
        ] {
            let dl_w_small = build_shifted_wide(10, width, step_x);
            let dl_w_large = build_shifted_wide(80, width, step_x);
            let (slide_ws, stats_ws) = display_list_to_slide_internal(
                &dl_w_small,
                0,
                &FontCache::new(),
                &ImageCache::new(),
                &ConversionOptions::default(),
                Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
                None,
            )
            .expect("small wide shifted export");
            let (slide_wl, stats_wl) = display_list_to_slide_internal(
                &dl_w_large,
                0,
                &FontCache::new(),
                &ImageCache::new(),
                &ConversionOptions::default(),
                Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
                None,
            )
            .expect("large wide shifted export");

            assert_eq!(slide_ws.elements.len(), 11);
            assert_eq!(slide_wl.elements.len(), 81);
            assert!(
                stats_wl.peak_backdrop_pixels <= MAX_BACKDROP_CACHE_PIXELS,
                "{label}: peak backdrop pixels must stay <= {MAX_BACKDROP_CACHE_PIXELS}, got {}",
                stats_wl.peak_backdrop_pixels
            );
            // Under per-crop windows, 8x overlapping fills would grow ~59x (3240 vs 55);
            // canonical coalesced windows keep replay strictly bounded (<= 4 windows per point).
            assert!(
                stats_wl.backdrop_command_visits <= stats_ws.backdrop_command_visits * 20,
                "{label}: shifted overlapping wide fills must scale linearly, small={} large={}",
                stats_ws.backdrop_command_visits,
                stats_wl.backdrop_command_visits
            );
            assert!(
                stats_wl.backdrop_command_visits <= 4 * (1 + 80 * 3) + 20,
                "{label}: coalesced windows must replay each overlapping fill at most 4x, got {}",
                stats_wl.backdrop_command_visits
            );
            let SlideElement::Image { image_data, .. } =
                slide_ws.elements.last().expect("wide fill image")
            else {
                panic!("{label}: expected SlideElement::Image");
            };
            let rgba = image::load_from_memory(image_data)
                .expect("decode wide PNG")
                .to_rgba8();
            let (w, h) = rgba.dimensions();
            assert!(
                w >= min_px_w,
                "{label}: wide crop must stay high resolution (>= {min_px_w}px wide), got {w}x{h}"
            );
            assert_eq!(
                rgba.get_pixel(w / 2, h / 2).0,
                [0, 0, 0, 255],
                "{label}: red Multiply over blue must blend to black"
            );
        }

        // Single 800pt-wide Multiply strip in a 2048pt grouping window must
        // choose its raster scale from the 802pt rendered union (2.0x -> ~1604px),
        // preserving an aligned 0.5pt gap as a clear transparent pixel column.
        let mut dl_gap = DisplayList::new(page_rect);
        let mut bg = zpdf_display_list::Path::new();
        bg.rect(page_rect);
        dl_gap.push(RenderCommand::FillPath {
            path: bg,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        let strip_bounds = Rect::new(100.0, 100.0, 900.0, 120.0);
        dl_gap.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: strip_bounds,
            alpha: 1.0,
            mask: None,
        });
        let mut left_half = zpdf_display_list::Path::new();
        left_half.rect(Rect::new(100.0, 100.0, 500.0, 120.0));
        dl_gap.push(RenderCommand::FillPath {
            path: left_half,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        let mut right_half = zpdf_display_list::Path::new();
        right_half.rect(Rect::new(500.5, 100.0, 900.0, 120.0));
        dl_gap.push(RenderCommand::FillPath {
            path: right_half,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl_gap.push(RenderCommand::PopBlendGroup);

        let (slide_gap, stats_gap) = display_list_to_slide_internal(
            &dl_gap,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
            Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
            None,
        )
        .expect("800pt gapped strip export");
        assert!(stats_gap.peak_backdrop_pixels <= MAX_BACKDROP_CACHE_PIXELS);
        assert_eq!(slide_gap.elements.len(), 2);
        let SlideElement::Image {
            x_emu, image_data, ..
        } = &slide_gap.elements[1]
        else {
            panic!("expected SlideElement::Image for 800pt strip");
        };
        let gap_rgba = image::load_from_memory(image_data)
            .expect("decode gapped strip PNG")
            .to_rgba8();
        let (gw, gh) = gap_rgba.dimensions();
        assert!(
            gw >= 1600,
            "800pt strip in 2048pt window must render at 2.0x (~1604px), got {gw}x{gh}"
        );
        let x0_pt = *x_emu as f64 / 12700.0;
        let gap_x = ((500.0 - x0_pt) * 2.0).round() as u32;
        assert_eq!(
            gap_rgba.get_pixel(gap_x, gh / 2).0[3],
            0,
            "aligned 0.5pt gap at x=500.0..500.5 must stay transparent at 2.0x, got {:?}",
            gap_rgba.get_pixel(gap_x, gh / 2).0
        );
        assert_eq!(
            gap_rgba.get_pixel(gap_x - 4, gh / 2).0,
            [0, 0, 0, 255],
            "left strip half must blend red Multiply over blue to black"
        );
        assert_eq!(
            gap_rgba.get_pixel(gap_x + 4, gh / 2).0,
            [0, 0, 0, 255],
            "right strip half must blend red Multiply over blue to black"
        );
    }

    #[test]
    fn fractional_soft_mask_offset_matches_between_standalone_and_white_multiply_backdrop() {
        let page_rect = Rect::new(0.0, 0.0, 120.0, 80.0);

        let build_and_check = |mask: zpdf_display_list::SoftMask, label: &str| {
            let mut dl = DisplayList::new(page_rect);
            // Red rectangle [10..50]x[10..50] under an Alpha soft mask whose left bar's
            // shifted right edge lands at x = 30.25 (half-pixel at 2x scale) while the
            // mask's union bounds span [5.25..55.25] so crop_rect stays aligned [9..51]^2.
            dl.push(RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: Rect::new(10.0, 10.0, 50.0, 50.0),
                alpha: 1.0,
                mask: Some(mask),
            });
            let mut red = zpdf_display_list::Path::new();
            red.rect(Rect::new(10.0, 10.0, 50.0, 50.0));
            dl.push(RenderCommand::FillPath {
                path: red,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
                alpha: 1.0,
                overprint: None,
            });
            dl.push(RenderCommand::PopBlendGroup);

            // White Multiply patch crossing the x = 30.25 fractional edge on the bottom half.
            let mul_rect = Rect::new(25.0, 10.0, 36.0, 30.0);
            dl.push(RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Multiply,
                isolated: true,
                knockout: false,
                bounds: mul_rect,
                alpha: 1.0,
                mask: None,
            });
            let mut patch = zpdf_display_list::Path::new();
            patch.rect(mul_rect);
            dl.push(RenderCommand::FillPath {
                path: patch,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: Paint::Solid(Color::white()),
                alpha: 1.0,
                overprint: None,
            });
            dl.push(RenderCommand::PopBlendGroup);

            let slide = display_list_to_slide(
                &dl,
                0,
                &FontCache::new(),
                &ImageCache::new(),
                &ConversionOptions::default(),
            )
            .expect("fractional soft-mask export");
            assert_eq!(slide.elements.len(), 2, "{label}: expected 2 images");
            let SlideElement::Image {
                x_emu: sx_emu,
                y_emu: sy_emu,
                image_data: standalone_png,
                ..
            } = &slide.elements[0]
            else {
                panic!("{label}: expected standalone SlideElement::Image");
            };
            let SlideElement::Image {
                x_emu: mx_emu,
                y_emu: my_emu,
                image_data: multiply_png,
                ..
            } = &slide.elements[1]
            else {
                panic!("{label}: expected multiply SlideElement::Image");
            };
            let standalone = image::load_from_memory(standalone_png)
                .expect("decode standalone PNG")
                .to_rgba8();
            let multiply = image::load_from_memory(multiply_png)
                .expect("decode multiply PNG")
                .to_rgba8();

            let sx_pt = *sx_emu as f64 / 12700.0;
            let sy_pt = *sy_emu as f64 / 12700.0;
            let mx_pt = *mx_emu as f64 / 12700.0;
            let my_pt = *my_emu as f64 / 12700.0;

            // Sample pixel covering PDF x in [30.0..30.5] (half covered by x <= 30.25)
            // at PDF y = 20.0 (PPT y = 60.0), which lies inside both images.
            let sample_x_pt = 30.0;
            let sample_y_ppt = 60.0;
            let s_px_x = ((sample_x_pt - sx_pt) * 2.0).round() as u32;
            let s_px_y = ((sample_y_ppt - sy_pt) * 2.0).round() as u32;
            let m_px_x = ((sample_x_pt - mx_pt) * 2.0).round() as u32;
            let m_px_y = ((sample_y_ppt - my_pt) * 2.0).round() as u32;

            let sp = standalone.get_pixel(s_px_x, s_px_y).0;
            // Composite straight-alpha standalone pixel over white page backdrop.
            let a = sp[3] as f32 / 255.0;
            let s_over_white = [
                (sp[0] as f32 * a + 255.0 * (1.0 - a)).round() as u8,
                (sp[1] as f32 * a + 255.0 * (1.0 - a)).round() as u8,
                (sp[2] as f32 * a + 255.0 * (1.0 - a)).round() as u8,
            ];
            let mp = multiply.get_pixel(m_px_x, m_px_y).0;

            // Both passes must exhibit fractional ~50% edge coverage (neither 0% nor 100%).
            assert!(
                (100..=156).contains(&s_over_white[1]),
                "{label}: standalone edge pixel must have ~50% fractional coverage over white, got {sp:?} -> {s_over_white:?}"
            );
            assert!(
                (100..=156).contains(&mp[1]),
                "{label}: white Multiply backdrop edge pixel must preserve ~50% fractional coverage, got {mp:?}"
            );
            for ch in 0..3 {
                assert!(
                    s_over_white[ch].abs_diff(mp[ch]) <= 1,
                    "{label}: channel {ch} mismatch between standalone-over-white ({s_over_white:?}) and white Multiply backdrop ({mp:?})"
                );
            }
        };

        // Case 1: Direct quarter-point offset (0.25, 0.0) at 2.0x scale (= 0.5 device px).
        let mut direct_dl = DisplayList::new(page_rect);
        let mut left_bar = zpdf_display_list::Path::new();
        left_bar.rect(Rect::new(5.0, 5.0, 30.0, 65.0));
        direct_dl.push(RenderCommand::FillPath {
            path: left_bar,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let mut right_bar = zpdf_display_list::Path::new();
        right_bar.rect(Rect::new(40.0, 5.0, 55.0, 65.0));
        direct_dl.push(RenderCommand::FillPath {
            path: right_bar,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        build_and_check(
            zpdf_display_list::SoftMask {
                kind: zpdf_display_list::SoftMaskKind::Alpha,
                commands: std::sync::Arc::new(direct_dl),
                offset: (0.25, 0.0),
                backdrop_luma: 0.0,
                transfer: None,
            },
            "direct fractional offset",
        );

        // Case 2: Device-pixel-aligned outer offset (2.0, 0.0) wrapping a nested mask
        // with fractional offset (0.25, 0.0) whose net left-bar right edge is still 30.25.
        let mut inner_dl = DisplayList::new(page_rect);
        let mut inner_left = zpdf_display_list::Path::new();
        inner_left.rect(Rect::new(3.0, 5.0, 28.0, 65.0));
        inner_dl.push(RenderCommand::FillPath {
            path: inner_left,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let mut inner_right = zpdf_display_list::Path::new();
        inner_right.rect(Rect::new(38.0, 5.0, 53.0, 65.0));
        inner_dl.push(RenderCommand::FillPath {
            path: inner_right,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let mut outer_dl = DisplayList::new(page_rect);
        outer_dl.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: page_rect,
            alpha: 1.0,
            mask: Some(zpdf_display_list::SoftMask {
                kind: zpdf_display_list::SoftMaskKind::Alpha,
                commands: std::sync::Arc::new(inner_dl),
                offset: (0.25, 0.0),
                backdrop_luma: 0.0,
                transfer: None,
            }),
        });
        let mut outer_fill = zpdf_display_list::Path::new();
        outer_fill.rect(Rect::new(3.0, 5.0, 53.0, 65.0));
        outer_dl.push(RenderCommand::FillPath {
            path: outer_fill,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        outer_dl.push(RenderCommand::PopBlendGroup);
        build_and_check(
            zpdf_display_list::SoftMask {
                kind: zpdf_display_list::SoftMaskKind::Alpha,
                commands: std::sync::Arc::new(outer_dl),
                offset: (2.0, 0.0),
                backdrop_luma: 0.0,
                transfer: None,
            },
            "nested fractional offset",
        );
    }

    #[test]
    fn translucent_form_with_many_off_rect_pattern_masks_streams_within_cache_cap() {
        let page_rect = Rect::new(0.0, 0.0, 200.0, 200.0);
        let mut off_page_mask_dl = DisplayList::new(Rect::new(-100.0, -100.0, 0.0, 0.0));
        let mut off_rect = zpdf_display_list::Path::new();
        off_rect.rect(Rect::new(-50.0, -50.0, -40.0, -40.0));
        off_page_mask_dl.push(RenderCommand::FillPath {
            path: off_rect,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let shared_off_page = std::sync::Arc::new(off_page_mask_dl);

        // A 50%-opacity Normal transparency Form takes the standalone flatten path
        // (no backdrop). Stream its 120 off-page masked pattern tiles without
        // retaining every translated mask alive simultaneously.
        let mut dl = DisplayList::new(page_rect);
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: page_rect,
            alpha: 0.5,
            mask: None,
        });
        for i in 0..120 {
            let target_x = if i == 119 {
                150.0f32
            } else {
                (i % 12) as f32 * 10.0
            };
            let target_y = if i == 119 {
                150.0f32
            } else {
                (i / 12) as f32 * 10.0
            };
            dl.push(RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: page_rect,
                alpha: 1.0,
                mask: Some(zpdf_display_list::SoftMask {
                    kind: zpdf_display_list::SoftMaskKind::Alpha,
                    commands: std::sync::Arc::clone(&shared_off_page),
                    offset: (target_x + 50.0, target_y + 50.0),
                    backdrop_luma: 0.0,
                    transfer: None,
                }),
            });
            let mut tile = zpdf_display_list::Path::new();
            tile.rect(Rect::new(
                target_x as f64,
                target_y as f64,
                target_x as f64 + 10.0,
                target_y as f64 + 10.0,
            ));
            dl.push(RenderCommand::FillPath {
                path: tile,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
                alpha: 1.0,
                overprint: None,
            });
            dl.push(RenderCommand::PopBlendGroup);
        }
        dl.push(RenderCommand::PopBlendGroup);

        let (slide, stats) = display_list_to_slide_internal(
            &dl,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
            Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
            None,
        )
        .expect("translucent form streaming export");

        assert!(
            stats.peak_flatten_rebaser_mask_entries <= MAX_TRANSLATED_MASK_ENTRIES,
            "standalone flatten rebaser must stay <= {MAX_TRANSLATED_MASK_ENTRIES}, got {}",
            stats.peak_flatten_rebaser_mask_entries
        );
        assert!(
            stats.peak_flatten_rebaser_mask_bytes <= MAX_TRANSLATED_MASK_BYTES,
            "standalone flatten rebaser bytes must stay <= {MAX_TRANSLATED_MASK_BYTES}, got {}",
            stats.peak_flatten_rebaser_mask_bytes
        );
        assert!(
            stats.flatten_cpu_mask_cache_invalidations >= 120 - MAX_TRANSLATED_MASK_ENTRIES,
            "post-cap streamed tiles must invalidate CpuRenderer soft-mask cache, got {} invalidations",
            stats.flatten_cpu_mask_cache_invalidations
        );
        assert_eq!(slide.elements.len(), 1);
        let SlideElement::Image {
            x_emu,
            y_emu,
            image_data,
            ..
        } = &slide.elements[0]
        else {
            panic!("expected SlideElement::Image for translucent Form");
        };
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        let x_pt = *x_emu as f64 / 12700.0;
        let y_pt = *y_emu as f64 / 12700.0;
        // Center of 120th post-cap tile [150..160]x[150..160] -> PDF (155, 155) -> PPT (155, 45).
        let tile_px = rgba
            .get_pixel(
                ((155.0 - x_pt) * 2.0).round() as u32,
                ((45.0 - y_pt) * 2.0).round() as u32,
            )
            .0;
        assert!(
            tile_px[2] > 240 && tile_px[0] < 15 && (115..=140).contains(&tile_px[3]),
            "120th post-cap tile inside translucent Form must render at 50% blue, got {tile_px:?}"
        );
        // Unpainted neighbor [150..160]x[130..140] -> PDF (155, 135) -> PPT (155, 65) stays transparent.
        let empty_px = rgba
            .get_pixel(
                ((155.0 - x_pt) * 2.0).round() as u32,
                ((65.0 - y_pt) * 2.0).round() as u32,
            )
            .0;
        assert_eq!(
            empty_px[3], 0,
            "unpainted neighbor outside post-cap tile must remain transparent, got {empty_px:?}"
        );
    }

    #[test]
    fn nested_soft_mask_with_large_child_obeys_retention_byte_limit() {
        let make_poly_mask = |num_vertices: usize| -> std::sync::Arc<DisplayList> {
            let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 200.0, 200.0));
            let mut path = zpdf_display_list::Path::new();
            path.move_to(zpdf_core::Point::new(0.0, 0.0));
            for i in 1..num_vertices {
                path.line_to(zpdf_core::Point::new(
                    (i % 100) as f64,
                    ((i / 100) % 100) as f64,
                ));
            }
            path.close();
            dl.push(RenderCommand::FillPath {
                path,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: Paint::Solid(Color::white()),
                alpha: 1.0,
                overprint: None,
            });
            std::sync::Arc::new(dl)
        };
        let wrap_child = |child: &std::sync::Arc<DisplayList>| -> std::sync::Arc<DisplayList> {
            let mut parent = DisplayList::new(Rect::new(0.0, 0.0, 200.0, 200.0));
            parent.push(RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 200.0, 200.0),
                alpha: 1.0,
                mask: Some(zpdf_display_list::SoftMask {
                    kind: zpdf_display_list::SoftMaskKind::Alpha,
                    commands: std::sync::Arc::clone(child),
                    offset: (1.0, 0.0),
                    backdrop_luma: 0.0,
                    transfer: None,
                }),
            });
            parent.push(RenderCommand::PopBlendGroup);
            std::sync::Arc::new(parent)
        };

        // Part A: Oversized child (> MAX_TRANSLATED_MASK_BYTES) wrapped in a tiny parent
        // must decline BOTH child and parent so the wrapper cannot retain oversized children.
        let oversized_child = make_poly_mask(MAX_TRANSLATED_MASK_BYTES / 24 + 1024);
        assert!(estimate_display_list_bytes(&oversized_child) > MAX_TRANSLATED_MASK_BYTES);
        let wrapper_oversized = wrap_child(&oversized_child);
        assert!(estimate_display_list_bytes(&wrapper_oversized) < 4096);

        let mut rebaser_a = SoftMaskRebaser::new(0.0, 200.0, 1.0, 1.0, 2.0);
        for step in 0..4 {
            let cmd = RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 200.0, 200.0),
                alpha: 1.0,
                mask: Some(zpdf_display_list::SoftMask {
                    kind: zpdf_display_list::SoftMaskKind::Alpha,
                    commands: std::sync::Arc::clone(&wrapper_oversized),
                    offset: (step as f32 * 10.0 + 5.0, 0.0),
                    backdrop_luma: 0.0,
                    transfer: None,
                }),
            };
            let _prepared = rebaser_a.prepare_command(&cmd);
            assert!(
                rebaser_a.take_needs_cpu_mask_cache_invalidation(),
                "placement {step} with oversized nested child must trigger cache invalidation"
            );
            assert_eq!(
                rebaser_a.translated_cache.len(),
                0,
                "wrapper around oversized child must NOT be admitted into translated_cache"
            );
            assert_eq!(rebaser_a.retained_bytes, 0);
        }

        // Part B: Medium child (~1.5 MiB) placed 5 times via a tiny wrapper parent must
        // stop admitting wrappers once child bytes hit MAX_TRANSLATED_MASK_BYTES (4 MiB).
        let medium_child = make_poly_mask(65_536); // ~1.5 MiB
        let child_est = estimate_display_list_bytes(&medium_child);
        assert!(
            child_est > 1_000_000 && child_est < 2_000_000,
            "expected ~1.5 MiB child, got {child_est}"
        );
        let wrapper_medium = wrap_child(&medium_child);

        let mut rebaser_b = SoftMaskRebaser::new(0.0, 200.0, 1.0, 1.0, 2.0);
        for step in 0..5 {
            let cmd = RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 200.0, 200.0),
                alpha: 1.0,
                mask: Some(zpdf_display_list::SoftMask {
                    kind: zpdf_display_list::SoftMaskKind::Alpha,
                    commands: std::sync::Arc::clone(&wrapper_medium),
                    offset: (step as f32 * 10.0 + 5.0, 0.0),
                    backdrop_luma: 0.0,
                    transfer: None,
                }),
            };
            let _prepared = rebaser_b.prepare_command(&cmd);
            let invalidated = rebaser_b.take_needs_cpu_mask_cache_invalidation();
            if step >= 2 {
                assert!(
                    invalidated,
                    "placement {step} exceeding 4 MiB child budget must decline wrapper and request invalidation"
                );
            }
        }
        assert!(
            rebaser_b.retained_bytes <= MAX_TRANSLATED_MASK_BYTES,
            "retained bytes must stay <= {MAX_TRANSLATED_MASK_BYTES}, got {}",
            rebaser_b.retained_bytes
        );
        // Only the first 2 placements (2 children + 2 wrappers = 4 entries, ~3 MiB) fit.
        assert!(
            rebaser_b.translated_cache.len() <= 4,
            "wrappers past the child byte cap must not be cached, got {} entries",
            rebaser_b.translated_cache.len()
        );
    }

    #[test]
    fn canceling_nested_soft_mask_offsets_match_zero_offset_rendering() {
        let page_rect = Rect::new(0.0, 0.0, 100.0, 100.0);

        let build_scene = |outer_offset: (f32, f32),
                           inner_offset: (f32, f32),
                           outer_fill_rect: Rect|
         -> DisplayList {
            let mut inner_dl = DisplayList::new(page_rect);
            let mut inner_path = zpdf_display_list::Path::new();
            inner_path.rect(Rect::new(20.0, 20.0, 60.0, 60.0));
            inner_dl.push(RenderCommand::FillPath {
                path: inner_path,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: Paint::Solid(Color::white()),
                alpha: 1.0,
                overprint: None,
            });

            let mut outer_dl = DisplayList::new(page_rect);
            outer_dl.push(RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: page_rect,
                alpha: 1.0,
                mask: Some(zpdf_display_list::SoftMask {
                    kind: zpdf_display_list::SoftMaskKind::Alpha,
                    commands: std::sync::Arc::new(inner_dl),
                    offset: inner_offset,
                    backdrop_luma: 0.0,
                    transfer: None,
                }),
            });
            let mut outer_path = zpdf_display_list::Path::new();
            outer_path.rect(outer_fill_rect);
            outer_dl.push(RenderCommand::FillPath {
                path: outer_path,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: Paint::Solid(Color::white()),
                alpha: 1.0,
                overprint: None,
            });
            outer_dl.push(RenderCommand::PopBlendGroup);

            let mut dl = DisplayList::new(page_rect);
            dl.push(RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: Rect::new(10.0, 10.0, 80.0, 80.0),
                alpha: 1.0,
                mask: Some(zpdf_display_list::SoftMask {
                    kind: zpdf_display_list::SoftMaskKind::Alpha,
                    commands: std::sync::Arc::new(outer_dl),
                    offset: outer_offset,
                    backdrop_luma: 0.0,
                    transfer: None,
                }),
            });
            let mut red = zpdf_display_list::Path::new();
            red.rect(Rect::new(10.0, 10.0, 80.0, 80.0));
            dl.push(RenderCommand::FillPath {
                path: red,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
                alpha: 1.0,
                overprint: None,
            });
            dl.push(RenderCommand::PopBlendGroup);

            // Trailing white Multiply patch covering [15..65]^2 to test the backdrop path too.
            let mul_rect = Rect::new(15.0, 15.0, 65.0, 65.0);
            dl.push(RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Multiply,
                isolated: true,
                knockout: false,
                bounds: mul_rect,
                alpha: 1.0,
                mask: None,
            });
            let mut patch = zpdf_display_list::Path::new();
            patch.rect(mul_rect);
            dl.push(RenderCommand::FillPath {
                path: patch,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: Paint::Solid(Color::white()),
                alpha: 1.0,
                overprint: None,
            });
            dl.push(RenderCommand::PopBlendGroup);
            dl
        };

        // Outer +8.25pt and inner -8.25pt cancel to net 0.0 on the inner mask [20..60]^2,
        // while outer fill [10..70] shifts to [18.25..78.25] (covering [20..60]).
        let dl_canceling =
            build_scene((8.25, 0.0), (-8.25, 0.0), Rect::new(10.0, 10.0, 70.0, 70.0));
        let dl_reference = build_scene((0.0, 0.0), (0.0, 0.0), Rect::new(18.25, 10.0, 78.25, 70.0));

        let slide_canceling = display_list_to_slide(
            &dl_canceling,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("canceling offset export");
        let slide_reference = display_list_to_slide(
            &dl_reference,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("reference zero-offset export");

        assert_eq!(slide_canceling.elements.len(), 2);
        assert_eq!(slide_reference.elements.len(), 2);

        for (idx, pass_name) in [(0, "standalone"), (1, "backdrop")] {
            let (
                SlideElement::Image {
                    x_emu: cx,
                    y_emu: cy,
                    width_emu: cw,
                    height_emu: ch,
                    image_data: c_png,
                    ..
                },
                SlideElement::Image {
                    x_emu: rx,
                    y_emu: ry,
                    width_emu: rw,
                    height_emu: rh,
                    image_data: r_png,
                    ..
                },
            ) = (
                &slide_canceling.elements[idx],
                &slide_reference.elements[idx],
            )
            else {
                panic!("{pass_name}: expected SlideElement::Image");
            };
            assert_eq!(
                (*cx, *cy, *cw, *ch),
                (*rx, *ry, *rw, *rh),
                "{pass_name}: image placement must match zero-offset reference"
            );
            let c_rgba = image::load_from_memory(c_png).expect("decode").to_rgba8();
            let r_rgba = image::load_from_memory(r_png).expect("decode").to_rgba8();
            assert_eq!(c_rgba.dimensions(), r_rgba.dimensions());
            assert_eq!(
                c_rgba.as_raw(),
                r_rgba.as_raw(),
                "{pass_name}: canceling +8.25/-8.25 nested mask must render identically to zero-offset reference"
            );
            // Spot-check [54..58] (would be erased if stale -8.25 offset remained) and [18.5..19.5]
            // (would be leaked if stale -8.25 offset remained).
            let x_pt = *cx as f64 / 12700.0;
            let y_pt = *cy as f64 / 12700.0;
            let inside_px = c_rgba
                .get_pixel(
                    ((56.0 - x_pt) * 2.0).round() as u32,
                    ((60.0 - y_pt) * 2.0).round() as u32,
                )
                .0;
            assert_eq!(
                inside_px,
                [255, 0, 0, 255],
                "{pass_name}: x=56 inside [20..60] must remain opaque red"
            );
            let outside_px = c_rgba
                .get_pixel(
                    ((19.0 - x_pt) * 2.0).round() as u32,
                    ((60.0 - y_pt) * 2.0).round() as u32,
                )
                .0;
            let expected_outside = if idx == 0 {
                [0, 0, 0, 0]
            } else {
                [255, 255, 255, 255]
            };
            assert_eq!(
                outside_px, expected_outside,
                "{pass_name}: x=19 outside [20..60] must remain unmasked ({expected_outside:?})"
            );
        }
    }

    #[test]
    fn queued_tiled_snapshots_share_caller_clip_stack_without_cloning_clip_paths() {
        let page_rect = Rect::new(0.0, 0.0, 8192.0, 8192.0);
        let mut dl = DisplayList::new(page_rect);

        // Detailed non-rectangular polygon clip (~4,000 vertices) covering [0..4000]x[0..4000]
        // minus a chamfered corner, so `clip_stack` is active for every queued tiled label.
        let mut detailed_clip = zpdf_display_list::Path::new();
        detailed_clip.move_to(zpdf_core::Point::new(0.0, 0.0));
        for i in 1..=1000 {
            detailed_clip.line_to(zpdf_core::Point::new(i as f64 * 4.0, 0.0));
        }
        for i in 1..=1000 {
            detailed_clip.line_to(zpdf_core::Point::new(4000.0, i as f64 * 3.9));
        }
        for i in 1..=1000 {
            detailed_clip.line_to(zpdf_core::Point::new(4000.0 - i as f64 * 4.0, 4000.0));
        }
        for i in 1..=1000 {
            detailed_clip.line_to(zpdf_core::Point::new(0.0, 4000.0 - i as f64 * 4.0));
        }
        detailed_clip.close();

        let push_label = |dl: &mut DisplayList, x0: f64, y0: f64| {
            let r = Rect::new(x0, y0, x0 + 16.0, y0 + 16.0);
            dl.push(RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Multiply,
                isolated: true,
                knockout: false,
                bounds: r,
                alpha: 1.0,
                mask: None,
            });
            let mut p = zpdf_display_list::Path::new();
            p.rect(r);
            dl.push(RenderCommand::FillPath {
                path: p,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
                alpha: 1.0,
                overprint: None,
            });
            dl.push(RenderCommand::PopBlendGroup);
        };

        // State 1: 60 labels across multiple tiles under the single detailed clip.
        dl.push(RenderCommand::PushClip {
            path: detailed_clip,
            rule: zpdf_display_list::FillRule::NonZero,
        });
        for i in 0..60 {
            let x0 = 20.0 + (i % 6) as f64 * 600.0;
            let y0 = 20.0 + (i / 6) as f64 * 40.0;
            push_label(&mut dl, x0, y0);
        }

        // State 2: nested inner clip covering [0..2000]^2 with 20 labels inside + 1 clipped out.
        let mut inner_clip = zpdf_display_list::Path::new();
        inner_clip.rect(Rect::new(0.0, 0.0, 2000.0, 2000.0));
        dl.push(RenderCommand::PushClip {
            path: inner_clip,
            rule: zpdf_display_list::FillRule::NonZero,
        });
        for i in 0..20 {
            push_label(
                &mut dl,
                50.0 + (i % 4) as f64 * 400.0,
                800.0 + (i / 4) as f64 * 40.0,
            );
        }
        // Outside inner clip [0..2000]^2 -> dropped cleanly without producing an element.
        push_label(&mut dl, 3000.0, 3000.0);
        dl.push(RenderCommand::PopClip);

        // State 3: back to the outer detailed clip with 20 more labels.
        for i in 0..20 {
            push_label(
                &mut dl,
                50.0 + (i % 4) as f64 * 600.0,
                1400.0 + (i / 4) as f64 * 40.0,
            );
        }
        dl.push(RenderCommand::PopClip);

        let (slide, stats) = display_list_to_slide_internal(
            &dl,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
            Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
            None,
        )
        .expect("shared clip stack export");

        assert_eq!(
            slide.elements.len(),
            100,
            "60 + 20 + 20 visible labels must export while the clipped-out label is omitted"
        );
        assert_eq!(
            stats.queued_clip_stack_allocations, 3,
            "100 queued snapshots across 3 clip states must allocate only 3 shared clip-stack handles, got {}",
            stats.queued_clip_stack_allocations
        );
    }

    #[test]
    fn type3_compound_glyph_preserves_nonzero_and_evenodd_fill_rules() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Five unmapped Type 3 glyphs:
        // A (/nested_nz): two same-direction nested rectangles filled with `f` (NonZero -> center filled).
        // B (/nested_eo): same two nested rectangles filled with `f*` (EvenOdd -> center hole).
        // C (/star_nz): single-subpath 5-pointed star filled with `f` (NonZero -> center filled).
        // D (/star_eo): same 5-pointed star filled with `f*` (EvenOdd -> center hole).
        // E (/simple_rect): single simple rectangle filled with `f` (stays native Rectangle).
        let star_path = b"500 900 m 200 100 l 900 600 l 100 600 l 800 100 l h ";
        let mut star_nz = b"1000 0 d0 ".to_vec();
        star_nz.extend_from_slice(star_path);
        star_nz.push(b'f');
        let mut star_eo = b"1000 0 d0 ".to_vec();
        star_eo.extend_from_slice(star_path);
        star_eo.extend_from_slice(b"f*");

        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 120] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"0 0 1 rg BT /F1 100 Tf 10 10 Td (ABCDE) Tj ET"), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 69 /Widths [1000 1000 1000 1000 1000] /Resources << >> >>"
                .to_vec(), // 5
            b"<< /nested_nz 8 0 R /nested_eo 9 0 R /star_nz 10 0 R /star_eo 11 0 R /simple_rect 12 0 R >>"
                .to_vec(), // 6
            b"<< /Type /Encoding /Differences [65 /nested_nz /nested_eo /star_nz /star_eo /simple_rect] >>"
                .to_vec(), // 7
            stream("", b"1000 0 d0 0 0 800 800 re 200 200 400 400 re f"), // 8
            stream("", b"1000 0 d0 0 0 800 800 re 200 200 400 400 re f*"), // 9
            stream("", &star_nz),                                          // 10
            stream("", &star_eo),                                          // 11
            stream("", b"1000 0 d0 0 0 800 800 re f"),                     // 12
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
            .expect("slide export");
        assert_eq!(slide.elements.len(), 5);

        let decode_img = |idx: usize, label: &str| -> image::RgbaImage {
            let SlideElement::Image { image_data, .. } = &slide.elements[idx] else {
                panic!(
                    "{label} must rasterize to preserve fill rule, got {:?}",
                    slide.elements[idx]
                );
            };
            image::load_from_memory(image_data)
                .expect("decode PNG")
                .to_rgba8()
        };

        let is_blue = |px: [u8; 4]| px[3] > 240 && px[2] > 240 && px[0] < 15 && px[1] < 15;

        // A (/nested_nz): outer ring AND center must both be opaque blue.
        let nested_nz = decode_img(0, "/nested_nz");
        let (w, h) = nested_nz.dimensions();
        assert!(
            is_blue(nested_nz.get_pixel(w / 8, h / 2).0),
            "/nested_nz outer ring must be blue, got {:?}",
            nested_nz.get_pixel(w / 8, h / 2).0
        );
        assert!(
            is_blue(nested_nz.get_pixel(w / 2, h / 2).0),
            "/nested_nz (f / NonZero) center must be filled blue, got {:?}",
            nested_nz.get_pixel(w / 2, h / 2).0
        );

        // B (/nested_eo): outer ring blue, center must be a transparent hole.
        let nested_eo = decode_img(1, "/nested_eo");
        let (w, h) = nested_eo.dimensions();
        assert!(
            is_blue(nested_eo.get_pixel(w / 8, h / 2).0),
            "/nested_eo outer ring must be blue, got {:?}",
            nested_eo.get_pixel(w / 8, h / 2).0
        );
        assert_eq!(
            nested_eo.get_pixel(w / 2, h / 2).0[3],
            0,
            "/nested_eo (f* / EvenOdd) center must be a transparent hole, got {:?}",
            nested_eo.get_pixel(w / 2, h / 2).0
        );

        // C (/star_nz): center pentagon filled blue.
        let star_nz_img = decode_img(2, "/star_nz");
        let (w, h) = star_nz_img.dimensions();
        assert!(
            is_blue(star_nz_img.get_pixel(w / 2, h / 2).0),
            "/star_nz (f / NonZero) star center must be filled blue, got {:?}",
            star_nz_img.get_pixel(w / 2, h / 2).0
        );

        // D (/star_eo): center pentagon transparent hole while top star tip is blue.
        let star_eo_img = decode_img(3, "/star_eo");
        let (w, h) = star_eo_img.dimensions();
        assert_eq!(
            star_eo_img.get_pixel(w / 2, h / 2).0[3],
            0,
            "/star_eo (f* / EvenOdd) star center must be a transparent hole, got {:?}",
            star_eo_img.get_pixel(w / 2, h / 2).0
        );
        assert!(
            is_blue(star_eo_img.get_pixel(w / 2, h / 6).0),
            "/star_eo outer top tip must be filled blue, got {:?}",
            star_eo_img.get_pixel(w / 2, h / 6).0
        );

        // E (/simple_rect): simple non-compound rectangle stays native SlideElement::Rectangle.
        assert!(
            matches!(
                slide.elements[4],
                SlideElement::Rectangle {
                    fill_rgb: Some((0, 0, 255)),
                    ..
                }
            ),
            "/simple_rect must remain native SlideElement::Rectangle, got {:?}",
            slide.elements[4]
        );
    }

    #[test]
    fn type3_four_cubic_notched_glyph_preserves_actual_outline_not_ellipse() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Four-cubic heart-like glyph with a deep inward top notch down to (500, 620)
        // while the two lobes peak near y=850 and the bottom tip sits at (500, 100).
        // `recognize_ellipse` would misclassify any closed 4-cubic path as an oval.
        let heart_curves = b"500 620 m \
            500 880 100 940 100 600 c \
            100 350 300 220 500 100 c \
            700 220 900 350 900 600 c \
            900 940 500 880 500 620 c h ";
        let mut filled_heart = b"1000 0 d0 ".to_vec();
        filled_heart.extend_from_slice(heart_curves);
        filled_heart.push(b'f');
        let mut stroked_heart = b"1000 0 d0 40 w ".to_vec();
        stroked_heart.extend_from_slice(heart_curves);
        stroked_heart.push(b'S');

        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 260 120] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"0 0 1 rg 0 0 1 RG BT /F1 100 Tf 10 10 Td (AB) Tj ET"), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 66 /Widths [1000 1000] /Resources << >> >>"
                .to_vec(), // 5
            b"<< /heart_fill 8 0 R /heart_stroke 9 0 R >>".to_vec(), // 6
            b"<< /Type /Encoding /Differences [65 /heart_fill /heart_stroke] >>".to_vec(), // 7
            stream("", &filled_heart),                     // 8
            stream("", &stroked_heart),                    // 9
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
            .expect("slide export");
        assert_eq!(slide.elements.len(), 2);
        assert!(
            !slide
                .elements
                .iter()
                .any(|e| matches!(e.base(), SlideElement::Ellipse { .. })),
            "four-cubic notched Type 3 glyph must NOT collapse into SlideElement::Ellipse: {:?}",
            slide.elements
        );

        let SlideElement::Image {
            x_emu,
            y_emu,
            image_data,
            ..
        } = &slide.elements[0]
        else {
            panic!(
                "filled four-cubic heart glyph must preserve actual outline via rasterization, got {:?}",
                slide.elements[0]
            );
        };
        let rgba = image::load_from_memory(image_data)
            .expect("decode heart PNG")
            .to_rgba8();
        let x_pt = *x_emu as f64 / 12700.0;
        let y_pt = *y_emu as f64 / 12700.0;
        let sample_pdf = |pdf_x: f64, pdf_y: f64| -> [u8; 4] {
            let ppt_y = 120.0 - pdf_y;
            let px = ((pdf_x - x_pt) * 2.0).round() as u32;
            let py = ((ppt_y - y_pt) * 2.0).round() as u32;
            rgba.get_pixel(px, py).0
        };
        let is_blue = |px: [u8; 4]| px[3] > 240 && px[2] > 240 && px[0] < 15 && px[1] < 15;

        // Top inward notch at glyph (500, 780) -> PDF (60, 88) must be transparent (NOT filled oval!).
        let notch_px = sample_pdf(60.0, 88.0);
        assert_eq!(
            notch_px[3], 0,
            "inward top notch of 4-cubic heart must stay transparent, got {notch_px:?}"
        );
        // Left lobe (300, 680) -> PDF (40, 78), right lobe (700, 680) -> PDF (80, 78),
        // and lower center (500, 420) -> PDF (60, 52) must be opaque blue.
        assert!(
            is_blue(sample_pdf(40.0, 78.0)),
            "left lobe must be blue, got {:?}",
            sample_pdf(40.0, 78.0)
        );
        assert!(
            is_blue(sample_pdf(80.0, 78.0)),
            "right lobe must be blue, got {:?}",
            sample_pdf(80.0, 78.0)
        );
        assert!(
            is_blue(sample_pdf(60.0, 52.0)),
            "lower center body must be blue, got {:?}",
            sample_pdf(60.0, 52.0)
        );

        // Stroked four-cubic heart also preserves its actual notched outline (and sharp
        // notch join) rather than collapsing into an oval.
        let SlideElement::Image {
            x_emu: sx_emu,
            y_emu: sy_emu,
            image_data: stroke_png,
            ..
        } = &slide.elements[1]
        else {
            panic!(
                "stroked four-cubic heart must rasterize to preserve curves and joins, got {:?}",
                slide.elements[1]
            );
        };
        let stroke_rgba = image::load_from_memory(stroke_png)
            .expect("decode stroked heart PNG")
            .to_rgba8();
        let sx_pt = *sx_emu as f64 / 12700.0;
        let sy_pt = *sy_emu as f64 / 12700.0;
        let sample_stroke_pdf = |pdf_x: f64, pdf_y: f64| -> [u8; 4] {
            let ppt_y = 120.0 - pdf_y;
            let px = ((pdf_x - sx_pt) * 2.0).round() as u32;
            let py = ((ppt_y - sy_pt) * 2.0).round() as u32;
            stroke_rgba.get_pixel(px, py).0
        };
        // Second glyph sits shifted +100pt in X: notch at (160, 88) and hollow center (160, 52)
        // must remain transparent, while the bottom tip stroke at (160, 20) is opaque blue.
        assert_eq!(
            sample_stroke_pdf(160.0, 88.0)[3],
            0,
            "stroked heart inward top notch must stay transparent"
        );
        assert_eq!(
            sample_stroke_pdf(160.0, 52.0)[3],
            0,
            "stroked heart interior must stay transparent"
        );
        assert!(
            is_blue(sample_stroke_pdf(160.0, 20.0)),
            "stroked heart bottom tip must be painted blue, got {:?}",
            sample_stroke_pdf(160.0, 20.0)
        );
    }

    #[test]
    fn type3_explicitly_closed_right_triangle_stays_freeform_while_rectangle_stays_native() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // /tri traces a right triangle, draws an explicit line back to (0, 0), and closes (`h`).
        // That yields 5 path elements (4 point entries with 2 unique Xs and 2 unique Ys),
        // which must NOT be misclassified as a Rectangle.
        // /rect is a genuine four-corner axis-aligned rectangle and must stay native Rectangle.
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 120] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"0 0 1 rg BT /F1 100 Tf 10 10 Td (AB) Tj ET"), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 66 /Widths [1000 1000] /Resources << >> >>"
                .to_vec(), // 5
            b"<< /tri 8 0 R /rect 9 0 R >>".to_vec(),      // 6
            b"<< /Type /Encoding /Differences [65 /tri /rect] >>".to_vec(), // 7
            stream("", b"1000 0 d0 0 0 m 800 0 l 0 800 l 0 0 l h f"), // 8
            stream("", b"1000 0 d0 0 0 800 800 re f"),     // 9
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
            .expect("slide export");
        assert_eq!(slide.elements.len(), 2);

        let SlideElement::FreeformPath {
            path_data,
            fill_rgb: Some((0, 0, 255)),
            ..
        } = &slide.elements[0]
        else {
            panic!(
                "explicitly closed right-triangle Type 3 glyph must stay FreeformPath, got {:?}",
                slide.elements[0]
            );
        };
        assert!(
            path_data.contains("10.00 110.00")
                && path_data.contains("90.00 110.00")
                && path_data.contains("10.00 30.00"),
            "triangle FreeformPath must retain its three vertices: {path_data}"
        );
        assert!(
            !path_data.contains("90.00 30.00"),
            "triangle FreeformPath must not synthesize the missing fourth rectangle corner: {path_data}"
        );

        assert!(
            matches!(
                slide.elements[1],
                SlideElement::Rectangle {
                    fill_rgb: Some((0, 0, 255)),
                    ..
                }
            ),
            "genuine axis-aligned Type 3 rectangle must stay native Rectangle, got {:?}",
            slide.elements[1]
        );
    }

    #[test]
    fn type3_d0_overprint_honors_zero_ink_noop_and_partial_colorant_backdrop() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Cyan page backdrop (`0 1 1 rg`), followed by four `d0` Type 3 glyphs
        // enabling `/op true /OP true /OPM 1` in DeviceCMYK:
        // A (/zero_fill): zero CMYK ink fill -> Overprint { active: 0 } -> must emit NOTHING.
        // B (/zero_stroke): zero CMYK ink stroke -> Overprint { active: 0 } -> must emit NOTHING.
        // C (/yellow_fill): yellow-only CMYK overprint fill -> C(1) + Y(1) = green over cyan!
        // D (/yellow_stroke): yellow-only CMYK overprint stroke -> C(1) + Y(1) = green over cyan!
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 440 120] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(), // 3
            stream(
                "",
                b"0 1 1 rg 0 0 440 120 re f BT /F1 100 Tf 10 10 Td (ABCD) Tj ET",
            ), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 68 /Widths [1000 1000 1000 1000] \
              /Resources << /ExtGState << /GSop 12 0 R >> >> >>"
                .to_vec(), // 5
            b"<< /zero_fill 8 0 R /zero_stroke 9 0 R /yellow_fill 10 0 R /yellow_stroke 11 0 R >>"
                .to_vec(), // 6
            b"<< /Type /Encoding /Differences [65 /zero_fill /zero_stroke /yellow_fill /yellow_stroke] >>"
                .to_vec(), // 7
            stream("", b"1000 0 d0 /GSop gs 0 0 0 0 k 0 0 800 800 re f"), // 8
            stream("", b"1000 0 d0 /GSop gs 0 0 0 0 K 200 w 100 400 m 700 400 l S"), // 9
            stream("", b"1000 0 d0 /GSop gs 0 0 1 0 k 0 0 800 800 re f"), // 10
            stream("", b"1000 0 d0 /GSop gs 0 0 1 0 K 200 w 100 400 m 700 400 l S"), // 11
            b"<< /Type /ExtGState /op true /OP true /OPM 1 >>".to_vec(),   // 12
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
            .expect("overprint slide export");
        // Only 3 elements: cyan background Rectangle + 2 yellow-over-cyan green overprint Images.
        // Neither zero-ink overprint glyph should emit a white knockout shape.
        assert_eq!(
            slide.elements.len(),
            3,
            "zero-ink overprint glyphs must leave slide untouched while partial-colorant glyphs composite over backdrop: {:?}",
            slide.elements
        );
        assert!(matches!(
            slide.elements[0],
            SlideElement::Rectangle {
                fill_rgb: Some((0, 255, 255)),
                ..
            }
        ));
        for (idx, label) in [(1, "/yellow_fill"), (2, "/yellow_stroke")] {
            let SlideElement::Image { image_data, .. } = &slide.elements[idx] else {
                panic!(
                    "{label} partial-colorant overprint must composite against backdrop as Image, got {:?}",
                    slide.elements[idx]
                );
            };
            let rgba = image::load_from_memory(image_data)
                .expect("decode overprint PNG")
                .to_rgba8();
            let (w, h) = rgba.dimensions();
            let center = rgba.get_pixel(w / 2, h / 2).0;
            assert!(
                center[3] == 255 && center[0] < 15 && center[1] > 240 && center[2] < 15,
                "{label} yellow overprint on cyan backdrop must composite to green, got {center:?}"
            );
        }
    }

    #[test]
    fn type3_opaque_strokes_preserve_caps_joins_and_miter_limit() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Five opaque stroked Type 3 glyphs at 100pt scale (1000 glyph units = 100pt):
        // A (/butt_bar): short thick bar [300..500] at y=500, width=200 (20pt), butt cap (0 J).
        // B (/square_bar): identical bar with projecting square cap (2 J -> extends to [200..600]).
        // C (/miter_v): acute V-corner (350,200)-(500,600)-(650,200), width=100, miter join (0 j 10 M).
        // D (/bevel_v): identical V-corner with bevel join (2 j 10 M).
        // E (/low_miter_v): identical V-corner with low miter limit (0 j 1.2 M -> bevel fallback).
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 560 120] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"0 0 1 RG BT /F1 100 Tf 10 10 Td (ABCDE) Tj ET"), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 69 /Widths [1000 1000 1000 1000 1000] /Resources << >> >>"
                .to_vec(), // 5
            b"<< /butt_bar 8 0 R /square_bar 9 0 R /miter_v 10 0 R /bevel_v 11 0 R /low_miter_v 12 0 R >>"
                .to_vec(), // 6
            b"<< /Type /Encoding /Differences [65 /butt_bar /square_bar /miter_v /bevel_v /low_miter_v] >>"
                .to_vec(), // 7
            stream("", b"1000 0 d0 200 w 0 J 300 500 m 500 500 l S"), // 8
            stream("", b"1000 0 d0 200 w 2 J 300 500 m 500 500 l S"), // 9
            stream("", b"1000 0 d0 100 w 0 j 10 M 350 200 m 500 600 l 650 200 l S"), // 10
            stream("", b"1000 0 d0 100 w 2 j 10 M 350 200 m 500 600 l 650 200 l S"), // 11
            stream("", b"1000 0 d0 100 w 0 j 1.2 M 350 200 m 500 600 l 650 200 l S"), // 12
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
            .expect("stroke cap/join slide export");
        assert_eq!(slide.elements.len(), 5);

        let is_blue = |px: [u8; 4]| px[3] > 240 && px[2] > 240 && px[0] < 15 && px[1] < 15;
        let sample_elem_pdf = |idx: usize, pdf_x: f64, pdf_y: f64, label: &str| -> [u8; 4] {
            let SlideElement::Image {
                x_emu,
                y_emu,
                image_data,
                ..
            } = &slide.elements[idx]
            else {
                panic!(
                    "{label} must rasterize to preserve cap/join/miter geometry, got {:?}",
                    slide.elements[idx]
                );
            };
            let rgba = image::load_from_memory(image_data)
                .expect("decode PNG")
                .to_rgba8();
            let x_pt = *x_emu as f64 / 12700.0;
            let y_pt = *y_emu as f64 / 12700.0;
            let ppt_y = 120.0 - pdf_y;
            let px_f = ((pdf_x - x_pt) * 2.0).round();
            let py_f = ((ppt_y - y_pt) * 2.0).round();
            if px_f < 0.0
                || py_f < 0.0
                || px_f as u32 >= rgba.width()
                || py_f as u32 >= rgba.height()
            {
                return [0, 0, 0, 0];
            }
            rgba.get_pixel(px_f as u32, py_f as u32).0
        };

        // A (/butt_bar): simple straight Butt-cap bar stays native Line spanning [40..60]pt.
        let SlideElement::Line {
            x1_emu,
            x2_emu,
            stroke_width_pt,
            ..
        } = &slide.elements[0]
        else {
            panic!(
                "butt-cap straight bar should stay native Line, got {:?}",
                slide.elements[0]
            );
        };
        assert!(((*x2_emu - *x1_emu) as f64 / 12700.0 - 20.0).abs() < 0.1);
        assert!((*stroke_width_pt - 20.0).abs() < 0.1);

        // B (/square_bar) sits at origin x=110pt: bar [140..160]pt, width=20pt -> square caps
        // extend half-width (10pt) to [130..170]pt, so x=134pt and x=166pt must be opaque blue!
        assert!(
            is_blue(sample_elem_pdf(1, 134.0, 60.0, "/square_bar")),
            "projecting square cap must paint left extension at x=134"
        );
        assert!(
            is_blue(sample_elem_pdf(1, 166.0, 60.0, "/square_bar")),
            "projecting square cap must paint right extension at x=166"
        );

        // C (/miter_v) origin x=210, D (/bevel_v) origin x=310, E (/low_miter_v) origin x=410.
        // Vertex is at glyph (500, 600) -> PDF y=70; miter spike projects ~14.2pt above y=70
        // (up to ~84.2), whereas bevel cuts flat near y=72. Sample at y=78 (inside miter spike!).
        assert!(
            is_blue(sample_elem_pdf(2, 260.0, 78.0, "/miter_v")),
            "high-limit Miter join must project its sharp spike to y=78"
        );
        assert_eq!(
            sample_elem_pdf(3, 360.0, 78.0, "/bevel_v")[3],
            0,
            "Bevel join must NOT project a miter spike to y=78"
        );
        assert_eq!(
            sample_elem_pdf(4, 460.0, 78.0, "/low_miter_v")[3],
            0,
            "low miter limit (1.2) must bevel the sharp corner instead of projecting to y=78"
        );
    }

    #[test]
    fn isolated_normal_form_with_overprinting_type3_glyph_composites_against_form_backdrop() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // 400x200 page with a magenta outer backdrop (`1 0 1 rg`) and an opaque
        // isolated Normal Form XObject (/Fm1) that paints a cyan rectangle [20..220]x[20..140]
        // and then a yellow-only overprinting Type 3 `d0` glyph at [60..124]x[40..104].
        // Flattening the isolated Form as a unit produces green (cyan + yellow overprint)
        // in the overlap without knocking out cyan or mixing with the outer magenta page.
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 200] /Contents 4 0 R \
              /Resources << /XObject << /Fm1 5 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"1 0 1 rg 0 0 400 200 re f /Fm1 Do"), // 4
            stream(
                "/Type /XObject /Subtype /Form /BBox [20 20 220 140] \
                 /Group << /Type /Group /S /Transparency /I true /K false >> \
                 /Resources << /Font << /F1 6 0 R >> >>",
                b"0 1 1 rg 20 20 200 120 re f BT /F1 80 Tf 60 40 Td (A) Tj ET",
            ), // 5
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 7 0 R /Encoding 8 0 R \
              /FirstChar 65 /LastChar 65 /Widths [1000] \
              /Resources << /ExtGState << /GSop 10 0 R >> >> >>"
                .to_vec(), // 6
            b"<< /ovp 9 0 R >>".to_vec(),                  // 7
            b"<< /Type /Encoding /Differences [65 /ovp] >>".to_vec(), // 8
            stream("", b"1000 0 d0 /GSop gs 0 0 1 0 k 0 0 800 800 re f"), // 9
            b"<< /Type /ExtGState /op true /OPM 1 >>".to_vec(), // 10
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
            .expect("isolated form overprint export");
        assert_eq!(
            slide.elements.len(),
            2,
            "outer magenta backdrop + one flattened isolated Form image expected: {:?}",
            slide.elements
        );
        let SlideElement::Image {
            x_emu,
            y_emu,
            image_data,
            ..
        } = &slide.elements[1]
        else {
            panic!(
                "isolated Normal Form with inner partial-colorant overprint must flatten as SlideElement::Image, got {:?}",
                slide.elements[1]
            );
        };
        let rgba = image::load_from_memory(image_data)
            .expect("decode Form PNG")
            .to_rgba8();
        let x_pt = *x_emu as f64 / 12700.0;
        let y_pt = *y_emu as f64 / 12700.0;
        let sample_pdf = |pdf_x: f64, pdf_y: f64| -> [u8; 4] {
            let ppt_y = 200.0 - pdf_y;
            let px = ((pdf_x - x_pt) * 2.0).round() as u32;
            let py = ((ppt_y - y_pt) * 2.0).round() as u32;
            rgba.get_pixel(px, py).0
        };

        // Inside the overlap [60..124]x[40..104] -> green (C=1 from Form's cyan + Y=1 from glyph).
        let overlap_px = sample_pdf(90.0, 70.0);
        assert!(
            overlap_px[3] == 255
                && overlap_px[0] < 15
                && overlap_px[1] > 240
                && overlap_px[2] < 15,
            "yellow overprint Type 3 glyph over cyan inside isolated Form must render green, got {overlap_px:?}"
        );
        // Inside the Form outside the glyph [20..60] -> pure cyan.
        let cyan_px = sample_pdf(35.0, 70.0);
        assert!(
            cyan_px[3] == 255 && cyan_px[0] < 15 && cyan_px[1] > 240 && cyan_px[2] > 240,
            "uncovered portion of isolated Form must remain cyan, got {cyan_px:?}"
        );
    }

    #[test]
    fn page_level_overprint_fill_stroke_and_text_composite_over_type3_artwork() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Three mapped cyan Type 3 glyphs (A at [20..84], B at [140..204], C at [250..314]),
        // overlapped respectively by:
        // 1. a page-level yellow-only overprinting rectangle [60..120]x[30..94]
        // 2. a page-level yellow-only overprinting thick stroke [180..235] at y=62
        // 3. a page-level yellow-only overprinting TrueType text run "(AA)" using a
        //    deterministic embedded/registered test face straddling x=314.
        const PRIMARY_TTF: &[u8] =
            include_bytes!("../../zpdf-font/tests/fixtures/coverage-primary.ttf");

        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 420 140] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R /F2 9 0 R >> /ExtGState << /GSop 10 0 R >> >> >>"
                .to_vec(), // 3
            stream(
                "",
                b"0 1 1 rg \
                  BT /F1 80 Tf 20 30 Td (A) Tj ET \
                  BT /F1 80 Tf 140 30 Td (B) Tj ET \
                  BT /F1 80 Tf 250 30 Td (C) Tj ET \
                  q /GSop gs 0 0 1 0 k 60 30 60 64 re f Q \
                  q /GSop gs 0 0 1 0 K 24 w 180 62 m 235 62 l S Q \
                  q /GSop gs 0 0 1 0 k BT /F2 72 Tf 270 36 Td (AA) Tj ET Q",
            ), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 67 /Widths [1000 1000 1000] /Resources << >> >>"
                .to_vec(), // 5
            b"<< /A 8 0 R /B 8 0 R /C 8 0 R >>".to_vec(),  // 6
            b"<< /Type /Encoding /Differences [65 /A /B /C] >>".to_vec(), // 7
            stream("", b"1000 0 d0 0 0 800 800 re f"),     // 8
            b"<< /Type /Font /Subtype /TrueType /BaseFont /PptxOverprintFace \
              /Encoding /WinAnsiEncoding >>"
                .to_vec(), // 9
            b"<< /Type /ExtGState /op true /OP true /OPM 1 >>".to_vec(), // 10
        ]);

        let provider = std::sync::Arc::new(zpdf_font::FontProvider::memory_only());
        provider
            .register_with_aliases(PRIMARY_TTF, 0, &["PptxOverprintFace"])
            .expect("register test face");
        let doc = zpdf_document::PdfDocument::open(pdf)
            .expect("open pdf")
            .with_font_provider(provider);
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
            .expect("page overprint export");
        // All 3 overlapped Type 3 glyphs fall back to cyan Rectangles, and all 3 page-level
        // overprints (fill, stroke, text) export as backdrop-composited Images -> 6 elements.
        assert_eq!(
            slide.elements.len(),
            6,
            "unexpected elements: {:?}",
            slide.elements
        );
        assert!(
            !slide
                .elements
                .iter()
                .any(|e| matches!(e.base(), SlideElement::TextBox { .. })),
            "overlapped Type 3 glyphs and overprinting page text must not export as plain TextBox"
        );

        let is_green = |px: [u8; 4]| px[3] == 255 && px[0] < 15 && px[1] > 240 && px[2] < 15;
        let is_yellow = |px: [u8; 4]| px[3] == 255 && px[0] > 240 && px[1] > 240 && px[2] < 15;

        let sample_img_pdf = |idx: usize, pdf_x: f64, pdf_y: f64, label: &str| -> [u8; 4] {
            let SlideElement::Image {
                x_emu,
                y_emu,
                image_data,
                ..
            } = &slide.elements[idx]
            else {
                panic!(
                    "{label}: expected SlideElement::Image, got {:?}",
                    slide.elements[idx]
                );
            };
            let rgba = image::load_from_memory(image_data)
                .expect("decode PNG")
                .to_rgba8();
            let x_pt = *x_emu as f64 / 12700.0;
            let y_pt = *y_emu as f64 / 12700.0;
            let ppt_y = 140.0 - pdf_y;
            rgba.get_pixel(
                ((pdf_x - x_pt) * 2.0).round() as u32,
                ((ppt_y - y_pt) * 2.0).round() as u32,
            )
            .0
        };

        // 1. Page-level overprint FillPath: [60..84] over cyan -> green, [90..115] over white -> yellow.
        assert!(
            is_green(sample_img_pdf(3, 72.0, 62.0, "page fill")),
            "page overprint fill over cyan Type 3 glyph must be green"
        );
        assert!(
            is_yellow(sample_img_pdf(3, 102.0, 62.0, "page fill")),
            "page overprint fill over white backdrop must stay yellow"
        );

        // 2. Page-level overprint StrokePath: [180..204] over cyan -> green, [210..230] over white -> yellow.
        assert!(
            is_green(sample_img_pdf(4, 192.0, 62.0, "page stroke")),
            "page overprint stroke over cyan Type 3 glyph must be green"
        );
        assert!(
            is_yellow(sample_img_pdf(4, 220.0, 62.0, "page stroke")),
            "page overprint stroke over white backdrop must stay yellow"
        );

        // 3. Page-level overprint DrawGlyphRun: "(MM)" overlaps cyan (C) [250..314] and extends onto white.
        let SlideElement::Image { image_data, .. } = &slide.elements[5] else {
            panic!(
                "page overprint text must export as SlideElement::Image, got {:?}",
                slide.elements[5]
            );
        };
        let text_rgba = image::load_from_memory(image_data)
            .expect("decode overprint text PNG")
            .to_rgba8();
        assert!(
            text_rgba.pixels().any(|p| is_green(p.0)),
            "page overprint text overlapping cyan Type 3 glyph must contain green pixels"
        );
        assert!(
            text_rgba.pixels().any(|p| is_yellow(p.0)),
            "page overprint text extending over white paper must contain yellow pixels"
        );
    }

    #[test]
    fn mixed_type3_run_with_local_overprint_keeps_unaffected_letter_editable() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Single `(AB) Tj` under caller DeviceGray `0.5 g` (no overprint):
        // - /A enables `/op true /OPM 1` locally without changing color -> identical RGB
        //   (0.5, 0.5, 0.5) and alpha 1.0 as caller, differing ONLY in `overprint` ->
        //   `overprint_matches` splits A out so it renders overprint artwork (dark cyan
        //   over the cyan backdrop).
        // - /B inherits caller's normal `0.5 g` -> stays an editable TextBox("B")!
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 120] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(), // 3
            stream(
                "",
                b"0 1 1 rg 0 0 240 120 re f 0.5 g BT /F1 60 Tf 20 30 Td (AB) Tj ET",
            ), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 66 /Widths [1000 1000] \
              /Resources << /ExtGState << /GSop 10 0 R >> >> >>"
                .to_vec(), // 5
            b"<< /A 8 0 R /B 9 0 R >>".to_vec(),           // 6
            b"<< /Type /Encoding /Differences [65 /A /B] >>".to_vec(), // 7
            stream("", b"1000 0 d0 /GSop gs 0 0 800 800 re f"), // 8
            stream("", b"1000 0 d0 0 0 800 800 re f"),     // 9
            b"<< /Type /ExtGState /op true /OPM 1 >>".to_vec(), // 10
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let mut spans = Vec::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .with_text_sink(&mut spans)
            .interpret_with_stats(&content_bytes);

        let extracted: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(extracted, "AB");

        let slide = display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
            .expect("mixed overprint run export");
        let texts: Vec<&str> = slide
            .elements
            .iter()
            .filter_map(|e| match e.base() {
                SlideElement::TextBox { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            vec!["B"],
            "unaffected letter B in (AB) Tj must remain an editable TextBox while overprinting A stays artwork: {:?}",
            slide.elements
        );
        let SlideElement::Image { image_data, .. } = slide
            .elements
            .iter()
            .find(|e| matches!(e.base(), SlideElement::Image { .. }))
            .expect("overprinting letter A must export as a backdrop-composited Image")
        else {
            unreachable!()
        };
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        let center = rgba.get_pixel(rgba.width() / 2, rgba.height() / 2).0;
        // 0.5 g (K=0.5) overprint on cyan (C=1) -> C=1, K=0.5 -> dark cyan ~(0, 128, 128).
        assert!(
            center[3] == 255
                && center[0] < 15
                && (115..=140).contains(&center[1])
                && (115..=140).contains(&center[2]),
            "0.5 g overprint letter A over cyan backdrop must composite to dark cyan, got {center:?}"
        );
    }

    #[test]
    fn overprinting_metrics_only_helvetica_stays_native_text_with_empty_provider() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Plain non-embedded Standard 14 Helvetica with `/op true /OPM 1` on white paper:
        // - `0 g (Hello)` on the page must stay a native black TextBox (not vanish in rasterizer).
        // - `0 g (InsideForm)` inside an opaque isolated Normal Form must also stay native black TextBox.
        // - `1 g (Hidden)` (zero ink under OPM 1 -> active == 0) must remain omitted.
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 320 120] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> /ExtGState << /GSop 6 0 R >> \
              /XObject << /Fm1 7 0 R >> >> >>"
                .to_vec(), // 3
            stream(
                "",
                b"q /GSop gs 0 g BT /F1 24 Tf 20 80 Td (Hello) Tj ET Q \
                  q /GSop gs 1 g BT /F1 24 Tf 20 20 Td (Hidden) Tj ET Q \
                  /Fm1 Do",
            ), // 4
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(), // 5
            b"<< /Type /ExtGState /op true /OPM 1 >>".to_vec(), // 6
            stream(
                "/Type /XObject /Subtype /Form /BBox [0 0 320 120] \
                 /Group << /Type /Group /S /Transparency /I true /K false >> \
                 /Resources << /Font << /F1 5 0 R >> /ExtGState << /GSop 6 0 R >> >>",
                b"q /GSop gs 0 g BT /F1 24 Tf 20 50 Td (InsideForm) Tj ET Q",
            ), // 7
        ]);

        let provider = std::sync::Arc::new(zpdf_font::FontProvider::memory_only());
        let doc = zpdf_document::PdfDocument::open(pdf)
            .expect("open pdf")
            .with_font_provider(provider);
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content_bytes);

        // Verify the empty provider gave a metrics-only Helvetica face without outline bytes.
        let run_font_id = dl
            .commands
            .iter()
            .find_map(|c| match c {
                RenderCommand::DrawGlyphRun(r) => Some(r.font_id),
                _ => None,
            })
            .expect("DrawGlyphRun present");
        assert!(
            !fonts
                .get(run_font_id)
                .expect("font loaded")
                .has_font_data(),
            "empty FontProvider::memory_only() must leave Standard 14 Helvetica without outline data"
        );

        let slide = display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
            .expect("metrics-only overprint export");
        let text_boxes: Vec<(&str, (u8, u8, u8))> = slide
            .elements
            .iter()
            .filter_map(|e| match e.base() {
                SlideElement::TextBox {
                    text, color_rgb, ..
                } => Some((text.as_str(), *color_rgb)),
                _ => None,
            })
            .collect();
        assert_eq!(
            text_boxes,
            vec![("Hello", (0, 0, 0)), ("InsideForm", (0, 0, 0))],
            "overprinting metrics-only Helvetica must preserve native black TextBoxes while omitting zero-ink text: {:?}",
            slide.elements
        );
    }

    #[test]
    fn outer_isolated_form_keeps_heading_editable_when_child_isolated_form_overprints() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Opaque isolated Normal outer Form (/OuterFm) contains:
        // 1. an ordinary editable heading "(Heading)" in Helvetica, and
        // 2. an opaque isolated Normal child Form (/InnerFm) containing a cyan rectangle
        //    and a yellow-only overprinting Type 3 glyph.
        // Because /InnerFm isolates and resolves its own overprint, the dependency must
        // stop at /InnerFm so /OuterFm is NOT flattened and "(Heading)" stays a TextBox.
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 200] /Contents 4 0 R \
              /Resources << /XObject << /OuterFm 5 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"/OuterFm Do"),                    // 4
            stream(
                "/Type /XObject /Subtype /Form /BBox [0 0 400 200] \
                 /Group << /Type /Group /S /Transparency /I true /K false >> \
                 /Resources << /Font << /FH 11 0 R >> /XObject << /InnerFm 6 0 R >> >>",
                b"0 0 0 rg BT /FH 24 Tf 20 160 Td (Heading) Tj ET /InnerFm Do",
            ), // 5
            stream(
                "/Type /XObject /Subtype /Form /BBox [20 20 220 140] \
                 /Group << /Type /Group /S /Transparency /I true /K false >> \
                 /Resources << /Font << /F1 7 0 R >> >>",
                b"0 1 1 rg 20 20 200 120 re f BT /F1 80 Tf 60 40 Td (A) Tj ET",
            ), // 6
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 8 0 R /Encoding 9 0 R \
              /FirstChar 65 /LastChar 65 /Widths [1000] \
              /Resources << /ExtGState << /GSop 12 0 R >> >> >>"
                .to_vec(), // 7
            b"<< /ovp 10 0 R >>".to_vec(),                 // 8
            b"<< /Type /Encoding /Differences [65 /ovp] >>".to_vec(), // 9
            stream("", b"1000 0 d0 /GSop gs 0 0 1 0 k 0 0 800 800 re f"), // 10
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(), // 11
            b"<< /Type /ExtGState /op true /OPM 1 >>".to_vec(), // 12
        ]);

        let provider = std::sync::Arc::new(zpdf_font::FontProvider::memory_only());
        let doc = zpdf_document::PdfDocument::open(pdf)
            .expect("open pdf")
            .with_font_provider(provider);
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
            .expect("nested isolated form export");
        assert_eq!(
            slide.elements.len(),
            2,
            "expected editable Heading TextBox + flattened child Form Image: {:?}",
            slide.elements
        );
        assert!(
            matches!(
                slide.elements[0].base(),
                SlideElement::TextBox { text, .. } if text == "Heading"
            ),
            "outer isolated Form heading must remain an editable TextBox: {:?}",
            slide.elements[0]
        );
        let SlideElement::Image {
            x_emu,
            y_emu,
            image_data,
            ..
        } = &slide.elements[1]
        else {
            panic!(
                "child isolated Form must flatten to SlideElement::Image, got {:?}",
                slide.elements[1]
            );
        };
        let rgba = image::load_from_memory(image_data)
            .expect("decode child Form PNG")
            .to_rgba8();
        let x_pt = *x_emu as f64 / 12700.0;
        let y_pt = *y_emu as f64 / 12700.0;
        let overlap_px = rgba
            .get_pixel(
                ((90.0 - x_pt) * 2.0).round() as u32,
                ((130.0 - y_pt) * 2.0).round() as u32,
            )
            .0;
        assert!(
            overlap_px[3] == 255 && overlap_px[0] < 15 && overlap_px[1] > 240 && overlap_px[2] < 15,
            "child isolated Form overprint overlap must be green, got {overlap_px:?}"
        );
    }

    #[test]
    fn later_yellow_overprint_preserves_earlier_metrics_only_black_helvetica_text() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        let open_and_export = |media_box: &str, page_ops: &[u8], form_ops: Option<&[u8]>| {
            let mut objs = vec![
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
                format!(
                    "<< /Type /Page /Parent 2 0 R /MediaBox [{media_box}] /Contents 4 0 R \
                     /Resources << /Font << /FH 5 0 R /FT3 6 0 R >> \
                     /ExtGState << /GSop 10 0 R >> \
                     /XObject << /Fm1 11 0 R >> >> >>"
                )
                .into_bytes(), // 3
                stream("", page_ops),                          // 4
                b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(), // 5
                b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 2000 1000] \
                  /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 7 0 R /Encoding 8 0 R \
                  /FirstChar 65 /LastChar 65 /Widths [2000] \
                  /Resources << /ExtGState << /GSop 10 0 R >> >> >>"
                    .to_vec(), // 6
                b"<< /ovp 9 0 R >>".to_vec(),                  // 7
                b"<< /Type /Encoding /Differences [65 /ovp] >>".to_vec(), // 8
                stream("", b"2000 0 d0 /GSop gs 0 0 1 0 k 0 0 2000 800 re f"), // 9
                b"<< /Type /ExtGState /op true /OPM 1 >>".to_vec(), // 10
                stream(
                    "/Type /XObject /Subtype /Form /BBox [10 10 240 120] \
                     /Group << /Type /Group /S /Transparency /I true /K false >> \
                     /Resources << /Font << /FH 5 0 R /FT3 6 0 R >> /ExtGState << /GSop 10 0 R >> >>",
                    form_ops.unwrap_or(b""),
                ), // 11
            ];
            let pdf = assemble(&objs);
            objs.clear();
            let provider = std::sync::Arc::new(zpdf_font::FontProvider::memory_only());
            let doc = zpdf_document::PdfDocument::open(pdf)
                .expect("open pdf")
                .with_font_provider(provider);
            let page = doc.page(0).expect("page 0");
            let mut fonts = doc.load_page_fonts(&page);
            let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
            let mut images = ImageCache::new();
            let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
                .with_document(doc.file(), &page.resources)
                .with_fonts(&mut fonts)
                .with_images(&mut images)
                .interpret_with_stats(&content_bytes);
            let slide_res =
                display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default());
            (dl, fonts, slide_res)
        };

        // 1. Standard page: cyan panel on left [0..55], black metrics-only Helvetica "(Hello)"
        //    at (20, 70), yellow-only overprinting Type 3 rectangle across [10..130, 60..108],
        //    then a later opaque blue rectangle at [140..190, 60..108].
        //    - Overprint image must sit BEHIND the native black "(Hello)" TextBox so the black ink
        //      remains visible through the yellow/green rectangle (and NOT hidden behind it).
        //    - Later blue rectangle must remain AFTER "(Hello)" in Z-order.
        let (dl, fonts, res) = open_and_export(
            "0 0 320 140",
            b"0 1 1 rg 0 0 55 140 re f \
              q /GSop gs 0 g BT /FH 24 Tf 20 70 Td (Hello) Tj ET Q \
              BT /FT3 60 Tf 10 60 Td (A) Tj ET \
              0 0 1 rg 140 60 50 48 re f",
            None,
        );
        let helv_fid = dl
            .commands
            .iter()
            .find_map(|c| match c {
                RenderCommand::DrawGlyphRun(r) => Some(r.font_id),
                _ => None,
            })
            .expect("Helvetica run");
        assert!(
            !fonts.get(helv_fid).expect("font").has_font_data(),
            "empty provider must yield outline-less Helvetica"
        );
        let slide =
            res.expect("yellow-only overprint over black metrics-only Helvetica must succeed");
        assert_eq!(
            slide.elements.len(),
            4,
            "expected cyan rect, overprint image, promoted Hello TextBox, later blue rect: {:?}",
            slide.elements
        );
        assert!(matches!(
            slide.elements[0].base(),
            SlideElement::Rectangle {
                fill_rgb: Some((0, 255, 255)),
                ..
            }
        ));
        let SlideElement::Image {
            x_emu,
            y_emu,
            image_data,
            ..
        } = slide.elements[1].base()
        else {
            panic!(
                "overprint image must precede promoted TextBox, got {:?}",
                slide.elements
            );
        };
        assert!(
            matches!(
                slide.elements[2].base(),
                SlideElement::TextBox {
                    text,
                    color_rgb: (0, 0, 0),
                    ..
                } if text == "Hello"
            ),
            "black Hello TextBox must sit on top of the overprint image: {:?}",
            slide.elements
        );
        assert!(
            matches!(
                slide.elements[3].base(),
                SlideElement::Rectangle {
                    fill_rgb: Some((0, 0, 255)),
                    ..
                }
            ),
            "later paint after overprint must stay on top of promoted TextBox: {:?}",
            slide.elements
        );
        let rgba = image::load_from_memory(image_data)
            .expect("decode PNG")
            .to_rgba8();
        let x_pt = *x_emu as f64 / 12700.0;
        let y_pt = *y_emu as f64 / 12700.0;
        // Left side of overprint over cyan -> green; right side over white -> yellow.
        let green_px = rgba
            .get_pixel(
                ((30.0 - x_pt) * 2.0).round() as u32,
                ((140.0 - 80.0 - y_pt) * 2.0).round() as u32,
            )
            .0;
        let yellow_px = rgba
            .get_pixel(
                ((90.0 - x_pt) * 2.0).round() as u32,
                ((140.0 - 80.0 - y_pt) * 2.0).round() as u32,
            )
            .0;
        assert!(
            green_px[3] == 255 && green_px[0] < 15 && green_px[1] > 240 && green_px[2] < 15,
            "overprint over cyan must stay green, got {green_px:?}"
        );
        assert!(
            yellow_px[3] == 255 && yellow_px[0] > 240 && yellow_px[1] > 240 && yellow_px[2] < 15,
            "overprint over white must stay yellow, got {yellow_px:?}"
        );

        // Verify serialized PPTX slide XML keeps <p:pic> before TextBox("Hello").
        let pptx_bytes = PptxWriter::new(PptxPresentation {
            slides: vec![slide],
            width_emu: pt_to_emu(320.0),
            height_emu: pt_to_emu(140.0),
        })
        .write_to_bytes()
        .expect("write pptx");
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(pptx_bytes)).expect("zip");
        let xml = String::from_utf8(zip_entry(&mut archive, "ppt/slides/slide1.xml")).expect("xml");
        let pic_pos = xml.find("<p:pic>").expect("picture element in slide XML");
        let text_pos = xml
            .find("<a:t>Hello</a:t>")
            .expect("Hello text in slide XML");
        assert!(
            pic_pos < text_pos,
            "yellow overprint <p:pic> must precede <a:t>Hello</a:t> in slide XML Z-order"
        );

        // 2. Opaque isolated Normal Form containing cyan backdrop + black metrics-only Helvetica
        //    + yellow-only overprint across the word: Form image + native black TextBox on top.
        let (_, _, form_res) = open_and_export(
            "0 0 320 140",
            b"/Fm1 Do",
            Some(
                b"0 1 1 rg 10 10 230 110 re f \
                  q /GSop gs 0 g BT /FH 24 Tf 20 50 Td (FormWord) Tj ET Q \
                  BT /FT3 60 Tf 15 40 Td (A) Tj ET",
            ),
        );
        let form_slide =
            form_res.expect("isolated Form with black metrics-only text + yellow overprint");
        assert_eq!(form_slide.elements.len(), 2, "{:?}", form_slide.elements);
        assert!(matches!(
            form_slide.elements[0].base(),
            SlideElement::Image { .. }
        ));
        assert!(matches!(
            form_slide.elements[1].base(),
            SlideElement::TextBox {
                text,
                color_rgb: (0, 0, 0),
                ..
            } if text == "FormWord"
        ));

        // 3. Large-sheet (4000x4000pt) tiled backdrop path: preserves exact Z-order
        //    [Image(yellow), TextBox("Tiled"), Rectangle(blue)].
        let (_, _, tiled_res) = open_and_export(
            "0 0 4000 4000",
            b"q /GSop gs 0 g BT /FH 24 Tf 20 70 Td (Tiled) Tj ET Q \
              BT /FT3 60 Tf 10 60 Td (A) Tj ET \
              0 0 1 rg 140 60 50 48 re f",
            None,
        );
        let tiled_slide = tiled_res.expect("tiled backdrop over black metrics-only Helvetica");
        assert_eq!(tiled_slide.elements.len(), 3, "{:?}", tiled_slide.elements);
        assert!(matches!(
            tiled_slide.elements[0].base(),
            SlideElement::Image { .. }
        ));
        assert!(matches!(
            tiled_slide.elements[1].base(),
            SlideElement::TextBox {
                text,
                color_rgb: (0, 0, 0),
                ..
            } if text == "Tiled"
        ));
        assert!(matches!(
            tiled_slide.elements[2].base(),
            SlideElement::Rectangle {
                fill_rgb: Some((0, 0, 255)),
                ..
            }
        ));

        // 4. Non-black (cyan) metrics-only Helvetica overlapped by yellow-only overprint
        //    would turn green in PDF and cannot be rendered faithfully without outlines -> Err.
        let (_, _, cyan_err) = open_and_export(
            "0 0 320 140",
            b"0 1 1 rg BT /FH 24 Tf 20 70 Td (Cyan) Tj ET \
              BT /FT3 60 Tf 10 60 Td (A) Tj ET",
            None,
        );
        assert!(
            cyan_err.is_err(),
            "yellow overprint over non-black metrics-only Helvetica must report unfaithful overlap"
        );

        // Same unfaithful overlap on the large-sheet tiled path must also report Err.
        let (_, _, tiled_cyan_err) = open_and_export(
            "0 0 4000 4000",
            b"0 1 1 rg BT /FH 24 Tf 20 70 Td (Cyan) Tj ET \
              BT /FT3 60 Tf 10 60 Td (A) Tj ET",
            None,
        );
        assert!(
            tiled_cyan_err.is_err(),
            "tiled yellow overprint over non-black metrics-only Helvetica must report Err"
        );

        // Disjoint non-black metrics-only Helvetica away from the overprint must still succeed.
        let (_, _, disjoint_ok) = open_and_export(
            "0 0 320 140",
            b"0 1 1 rg BT /FH 24 Tf 20 15 Td (Aside) Tj ET \
              BT /FT3 60 Tf 10 60 Td (A) Tj ET",
            None,
        );
        assert!(
            disjoint_ok.is_ok(),
            "non-overlapping cyan metrics-only Helvetica must export cleanly: {:?}",
            disjoint_ok.err()
        );

        // 5. Black metrics-only Helvetica partially occluded by an intervening opaque red
        //    rectangle before a later yellow-only overprint cannot reorder cleanly -> Err,
        //    while completely covering it with an opaque background rectangle succeeds.
        let (_, _, partial_occl_err) = open_and_export(
            "0 0 320 140",
            b"0 g BT /FH 24 Tf 20 70 Td (Hello) Tj ET \
              1 0 0 rg 50 60 40 48 re f \
              BT /FT3 60 Tf 10 60 Td (A) Tj ET",
            None,
        );
        assert!(
            partial_occl_err.is_err(),
            "partially occluded metrics-only black text under later overprint must report Err"
        );

        let (_, _, full_cover_ok) = open_and_export(
            "0 0 320 140",
            b"0 g BT /FH 24 Tf 20 70 Td (Covered) Tj ET \
              1 1 1 rg 0 0 320 140 re f \
              BT /FT3 60 Tf 10 60 Td (A) Tj ET",
            None,
        );
        assert!(
            full_cover_ok.is_ok(),
            "fully covered metrics-only text beneath opaque background wipe must succeed: {:?}",
            full_cover_ok.err()
        );
    }

    #[test]
    fn overprinting_metrics_only_helvetica_resolves_uniform_backdrop_and_rejects_non_uniform() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        let run_case = |page_ops: &[u8], form_ops: Option<&[u8]>| {
            let pdf = assemble(&[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 320 120] /Contents 4 0 R \
                  /Resources << /Font << /FH 5 0 R >> /ExtGState << /GSop 6 0 R >> \
                  /XObject << /Fm1 7 0 R >> >> >>"
                    .to_vec(), // 3
                stream("", page_ops),                          // 4
                b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(), // 5
                b"<< /Type /ExtGState /op true /OPM 1 >>".to_vec(), // 6
                stream(
                    "/Type /XObject /Subtype /Form /BBox [0 0 320 120] \
                     /Group << /Type /Group /S /Transparency /I true /K false >> \
                     /Resources << /Font << /FH 5 0 R >> /ExtGState << /GSop 6 0 R >> >>",
                    form_ops.unwrap_or(b""),
                ), // 7
            ]);
            let provider = std::sync::Arc::new(zpdf_font::FontProvider::memory_only());
            let doc = zpdf_document::PdfDocument::open(pdf)
                .expect("open pdf")
                .with_font_provider(provider);
            let page = doc.page(0).expect("page 0");
            let mut fonts = doc.load_page_fonts(&page);
            let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
            let mut images = ImageCache::new();
            let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
                .with_document(doc.file(), &page.resources)
                .with_fonts(&mut fonts)
                .with_images(&mut images)
                .interpret_with_stats(&content_bytes);
            display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
        };

        // 1. Uniform opaque cyan backdrop + 0.5 g /op true Helvetica -> dark cyan (0, 128, 128).
        let cyan_slide = run_case(
            b"0 1 1 rg 0 0 320 120 re f \
              q /GSop gs 0.5 g BT /FH 24 Tf 20 50 Td (Hello) Tj ET Q",
            None,
        )
        .expect("uniform cyan backdrop overprint");
        let text_colors: Vec<(&str, (u8, u8, u8))> = cyan_slide
            .elements
            .iter()
            .filter_map(|e| match e.base() {
                SlideElement::TextBox {
                    text, color_rgb, ..
                } => Some((text.as_str(), *color_rgb)),
                _ => None,
            })
            .collect();
        assert_eq!(
            text_colors,
            vec![("Hello", (0, 128, 128))],
            "0.5 g overprint Helvetica over cyan must resolve to dark cyan (0, 128, 128), not source gray"
        );

        // 2. Inside an isolated Normal Form over a magenta outer page:
        //    - Over transparent Form backdrop -> resolves against white paper -> (128, 128, 128).
        //    - Over a cyan rectangle inside the Form -> resolves to dark cyan (0, 128, 128).
        let iso_white = run_case(
            b"1 0 1 rg 0 0 320 120 re f /Fm1 Do",
            Some(b"q /GSop gs 0.5 g BT /FH 24 Tf 20 50 Td (IsoWhite) Tj ET Q"),
        )
        .expect("isolated Form overprint on empty Form backdrop");
        let iso_white_colors: Vec<(&str, (u8, u8, u8))> = iso_white
            .elements
            .iter()
            .filter_map(|e| match e.base() {
                SlideElement::TextBox {
                    text, color_rgb, ..
                } => Some((text.as_str(), *color_rgb)),
                _ => None,
            })
            .collect();
        assert_eq!(iso_white_colors, vec![("IsoWhite", (128, 128, 128))]);

        let iso_cyan = run_case(
            b"1 0 1 rg 0 0 320 120 re f /Fm1 Do",
            Some(
                b"0 1 1 rg 0 0 320 120 re f \
                  q /GSop gs 0.5 g BT /FH 24 Tf 20 50 Td (IsoCyan) Tj ET Q",
            ),
        )
        .expect("isolated Form overprint on Form cyan backdrop");
        let iso_cyan_colors: Vec<(&str, (u8, u8, u8))> = iso_cyan
            .elements
            .iter()
            .filter_map(|e| match e.base() {
                SlideElement::TextBox {
                    text, color_rgb, ..
                } => Some((text.as_str(), *color_rgb)),
                _ => None,
            })
            .collect();
        assert_eq!(iso_cyan_colors, vec![("IsoCyan", (0, 128, 128))]);

        // 3. Split cyan/white backdrop under 0.5 g /op true Helvetica -> non-uniform -> Err.
        let split_err = run_case(
            b"0 1 1 rg 0 0 50 120 re f \
              q /GSop gs 0.5 g BT /FH 24 Tf 20 50 Td (SplitWord) Tj ET Q",
            None,
        );
        assert!(
            split_err.is_err(),
            "0.5 g overprint Helvetica straddling cyan and white must report non-uniform backdrop error"
        );

        // 4. Opaque 0 g /op true (K=1) over the same split cyan/white backdrop resolves
        //    uniformly to pure black (0, 0, 0) regardless of backdrop -> Ok.
        let split_black = run_case(
            b"0 1 1 rg 0 0 50 120 re f \
              q /GSop gs 0 g BT /FH 24 Tf 20 50 Td (BlackSplit) Tj ET Q",
            None,
        )
        .expect("opaque K=1 overprint over split backdrop is backdrop-independent black");
        let black_colors: Vec<(&str, (u8, u8, u8))> = split_black
            .elements
            .iter()
            .filter_map(|e| match e.base() {
                SlideElement::TextBox {
                    text, color_rgb, ..
                } => Some((text.as_str(), *color_rgb)),
                _ => None,
            })
            .collect();
        assert_eq!(black_colors, vec![("BlackSplit", (0, 0, 0))]);
    }

    #[test]
    fn transparent_overprint_fill_does_not_flatten_isolated_form_or_editable_type3_run() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // 1. Opaque isolated Normal Form (/Fm1) containing:
        //    - metrics-only Helvetica heading "(Heading)", and
        //    - a fully transparent (`/ca 0`) yellow overprint fill (`/op true /OPM 1`, `0 0 1 0 k`).
        //    Because effective paint alpha is 0, the Form must NOT flatten and "(Heading)"
        //    must survive as an editable TextBox with zero raster images.
        // 2. Page-level mapped Type 3 word "(A)" followed by an overlapping `/ca 0` yellow
        //    overprint fill: "(A)" must remain an editable TextBox (not forced to artwork).
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 320 140] /Contents 4 0 R \
              /Resources << /Font << /FH 5 0 R /FT3 7 0 R >> \
              /ExtGState << /GSzero 6 0 R >> /XObject << /Fm1 11 0 R >> >> >>"
                .to_vec(), // 3
            stream(
                "",
                b"/Fm1 Do \
                  0 1 1 rg BT /FT3 40 Tf 20 20 Td (A) Tj ET \
                  q /GSzero gs 0 0 1 0 k 10 10 100 80 re f Q",
            ), // 4
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(), // 5
            b"<< /Type /ExtGState /ca 0 /op true /OPM 1 >>".to_vec(), // 6
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 8 0 R /Encoding 9 0 R \
              /FirstChar 65 /LastChar 65 /Widths [1000] /Resources << >> >>"
                .to_vec(), // 7
            b"<< /A 10 0 R >>".to_vec(),                   // 8
            b"<< /Type /Encoding /Differences [65 /A] >>".to_vec(), // 9
            stream("", b"1000 0 d1 0 0 800 800 0 0 800 800 re f"), // 10
            stream(
                "/Type /XObject /Subtype /Form /BBox [0 0 320 140] \
                 /Group << /Type /Group /S /Transparency /I true /K false >> \
                 /Resources << /Font << /FH 5 0 R >> /ExtGState << /GSzero 6 0 R >> >>",
                b"0 g BT /FH 24 Tf 20 90 Td (Heading) Tj ET \
                  q /GSzero gs 0 0 1 0 k 0 0 320 140 re f Q",
            ), // 11
        ]);

        let provider = std::sync::Arc::new(zpdf_font::FontProvider::memory_only());
        let doc = zpdf_document::PdfDocument::open(pdf)
            .expect("open pdf")
            .with_font_provider(provider);
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
            .expect("transparent overprint export");
        assert!(
            !slide
                .elements
                .iter()
                .any(|e| matches!(e.base(), SlideElement::Image { .. })),
            "fully transparent (/ca 0) overprint fill must not produce raster images: {:?}",
            slide.elements
        );
        let texts: Vec<&str> = slide
            .elements
            .iter()
            .filter_map(|e| match e.base() {
                SlideElement::TextBox { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            vec!["Heading", "A"],
            "isolated Form heading and overlapped mapped Type 3 letter must both remain editable TextBoxes: {:?}",
            slide.elements
        );
    }

    #[test]
    fn unmapped_type3_nested_outline_less_helvetica_skips_when_clipped_and_errors_when_visible() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        let run_page = |page_ops: &[u8]| {
            // Unmapped Type 3 CharProc (/icon at code 65) draws "A" using non-embedded Helvetica.
            let pdf = assemble(&[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 120] /Contents 4 0 R \
                  /Resources << /Font << /FT3 5 0 R >> >> >>"
                    .to_vec(), // 3
                stream("", page_ops),                          // 4
                b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
                  /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
                  /FirstChar 65 /LastChar 65 /Widths [1000] \
                  /Resources << /Font << /FH 9 0 R >> >> >>"
                    .to_vec(), // 5
                b"<< /icon 8 0 R >>".to_vec(),                 // 6
                b"<< /Type /Encoding /Differences [65 /icon] >>".to_vec(), // 7
                stream("", b"1000 0 d0 BT /FH 800 Tf 0 0 Td (A) Tj ET"), // 8
                b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(), // 9
            ]);
            let provider = std::sync::Arc::new(zpdf_font::FontProvider::memory_only());
            let doc = zpdf_document::PdfDocument::open(pdf)
                .expect("open pdf")
                .with_font_provider(provider);
            let page = doc.page(0).expect("page 0");
            let mut fonts = doc.load_page_fonts(&page);
            let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
            let mut images = ImageCache::new();
            let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
                .with_document(doc.file(), &page.resources)
                .with_fonts(&mut fonts)
                .with_images(&mut images)
                .interpret_with_stats(&content_bytes);
            display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
        };

        // 1. Disjoint caller clip entirely outside glyph [20..80, 20..80]: must emit nothing.
        let clipped = run_page(b"q 160 20 40 40 re W n BT /FT3 60 Tf 20 20 Td (A) Tj ET Q")
            .expect("disjoint clipped nested outline-less glyph must skip cleanly");
        assert!(
            clipped.elements.is_empty(),
            "disjointly clipped unmapped Type 3 glyph must not leak editable text: {:?}",
            clipped.elements
        );

        // 2. Visible unmapped Type 3 glyph drawing outline-less Helvetica must report missing
        //    outlines rather than leaking an editable slide TextBox.
        let visible = run_page(b"BT /FT3 60 Tf 20 20 Td (A) Tj ET");
        assert!(
            visible.is_err(),
            "visible unmapped Type 3 glyph with outline-less nested Helvetica must not export as slide TextBox"
        );
    }

    #[test]
    fn translucent_k_overprint_first_in_isolated_form_preserves_opacity_matching_cpu_renderer() {
        use zpdf_render::{PageRenderInfo, RenderBackend};

        const PRIMARY_TTF: &[u8] =
            include_bytes!("../../zpdf-font/tests/fixtures/coverage-primary.ttf");

        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Magenta page + isolated Normal Form whose FIRST paint is 50%-opaque K-only
        // overprinting (`0 g`, `/ca 0.5 /op true /OPM 1`) Helvetica "(AA)".
        let build_pdf = |font_obj: Vec<u8>| {
            assemble(&[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 120] /Contents 4 0 R \
                  /Resources << /XObject << /Fm1 5 0 R >> >> >>"
                    .to_vec(), // 3
                stream("", b"1 0 1 rg 0 0 240 120 re f /Fm1 Do"), // 4
                stream(
                    "/Type /XObject /Subtype /Form /BBox [0 0 240 120] \
                     /Group << /Type /Group /S /Transparency /I true /K false >> \
                     /Resources << /Font << /FH 6 0 R >> /ExtGState << /GShalf 7 0 R >> >>",
                    b"q /GShalf gs 0 g BT /FH 64 Tf 20 30 Td (AA) Tj ET Q",
                ), // 5
                font_obj,                                      // 6
                b"<< /Type /ExtGState /ca 0.5 /op true /OPM 1 >>".to_vec(), // 7
            ])
        };

        // 1. Ground truth: render the same PDF with drawable outlines on CpuRenderer.
        let outline_provider = std::sync::Arc::new(zpdf_font::FontProvider::memory_only());
        outline_provider
            .register_with_aliases(PRIMARY_TTF, 0, &["PptxIsoOpFace"])
            .expect("register face");
        let outline_pdf = build_pdf(
            b"<< /Type /Font /Subtype /TrueType /BaseFont /PptxIsoOpFace \
              /Encoding /WinAnsiEncoding >>"
                .to_vec(),
        );
        let outline_doc = zpdf_document::PdfDocument::open(outline_pdf)
            .expect("open outline pdf")
            .with_font_provider(outline_provider);
        let outline_page = outline_doc.page(0).expect("page 0");
        let mut outline_fonts = outline_doc.load_page_fonts(&outline_page);
        let (outline_content, _) = outline_doc
            .page_content_with_diagnostics(&outline_page)
            .expect("content");
        let mut outline_images = ImageCache::new();
        let (outline_dl, _) =
            zpdf_content::interpreter::ContentInterpreter::new(outline_page.effective_box())
                .with_document(outline_doc.file(), &outline_page.resources)
                .with_fonts(&mut outline_fonts)
                .with_images(&mut outline_images)
                .interpret_with_stats(&outline_content);
        let mut cpu = zpdf_render_cpu::CpuRenderer::new()
            .with_fonts(&outline_fonts)
            .with_images(&outline_images);
        cpu.begin_page(&PageRenderInfo {
            page_rect: outline_page.effective_box(),
            scale: 2.0,
            background: zpdf_display_list::Color::white(),
        })
        .expect("begin_page");
        for cmd in &outline_dl.commands {
            cpu.execute(cmd).expect("execute");
        }
        let cpu_rendered = cpu.end_page().expect("end_page");
        // Find a painted interior glyph pixel (differs from magenta (255, 0, 255) and has
        // max green channel, i.e., full glyph interior coverage).
        let cpu_glyph_px = cpu_rendered
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .copied()
            .filter(|px| px[1] > 20)
            .max_by_key(|px| px[1])
            .expect("rendered glyph pixel");

        // 2. Empty-provider metrics-only Helvetica exported to PPTX.
        let empty_provider = std::sync::Arc::new(zpdf_font::FontProvider::memory_only());
        let helv_pdf =
            build_pdf(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec());
        let helv_doc = zpdf_document::PdfDocument::open(helv_pdf)
            .expect("open helv pdf")
            .with_font_provider(empty_provider);
        let helv_page = helv_doc.page(0).expect("page 0");
        let mut helv_fonts = helv_doc.load_page_fonts(&helv_page);
        let (helv_content, _) = helv_doc
            .page_content_with_diagnostics(&helv_page)
            .expect("content");
        let mut helv_images = ImageCache::new();
        let (helv_dl, _) =
            zpdf_content::interpreter::ContentInterpreter::new(helv_page.effective_box())
                .with_document(helv_doc.file(), &helv_page.resources)
                .with_fonts(&mut helv_fonts)
                .with_images(&mut helv_images)
                .interpret_with_stats(&helv_content);

        let slide = display_list_to_slide(
            &helv_dl,
            0,
            &helv_fonts,
            &helv_images,
            &ConversionOptions::default(),
        )
        .expect("isolated form 50%-opaque overprint export");
        assert_eq!(slide.elements.len(), 2, "{:?}", slide.elements);
        let SlideElement::Opacity { element, alpha } = &slide.elements[1] else {
            panic!(
                "50%-opaque K-only overprint first in transparent isolated Form must carry SlideElement::Opacity, got {:?}",
                slide.elements[1]
            );
        };
        assert!(
            (*alpha - 0.5).abs() <= 1e-3,
            "expected text opacity ~0.5, got {alpha}"
        );
        let SlideElement::TextBox {
            text, color_rgb, ..
        } = element.base()
        else {
            panic!("expected TextBox inside Opacity, got {element:?}");
        };
        assert_eq!(text, "AA");
        // Composite the exported TextBox + Opacity over the magenta page (255, 0, 255)
        // and verify it matches CpuRenderer with drawable outlines.
        let comp = [
            (color_rgb.0 as f32 * *alpha + 255.0 * (1.0 - *alpha)).round() as u8,
            (color_rgb.1 as f32 * *alpha).round() as u8,
            (color_rgb.2 as f32 * *alpha + 255.0 * (1.0 - *alpha)).round() as u8,
        ];
        for ch in 0..3 {
            assert!(
                comp[ch].abs_diff(cpu_glyph_px[ch]) <= 2,
                "composited channel {ch}: PPTX {comp:?} must match CpuRenderer {cpu_glyph_px:?}"
            );
        }
    }

    #[test]
    fn empty_alpha_mask_with_outline_less_helvetica_skips_group_without_error() {
        let page_rect = Rect::new(0.0, 0.0, 200.0, 100.0);
        let mut fonts = FontCache::new();
        let fid = fonts.insert(
            "FH".into(),
            zpdf_font::LoadedFont::new_placeholder("Helvetica".into()),
        );
        assert!(!fonts.get(fid).expect("font").has_font_data());

        let empty_mask = zpdf_display_list::SoftMask {
            kind: zpdf_display_list::SoftMaskKind::Alpha,
            commands: std::sync::Arc::new(DisplayList::new(page_rect)),
            offset: (0.0, 0.0),
            backdrop_luma: 0.0,
            transfer: None,
        };

        let mut dl = DisplayList::new(page_rect);
        // 1. Visible sibling rectangle.
        let mut rect = zpdf_display_list::Path::new();
        rect.rect(Rect::new(10.0, 10.0, 60.0, 50.0));
        dl.push(RenderCommand::FillPath {
            path: rect,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        // 2. Outline-less Helvetica inside an empty `/S /Alpha` soft-masked group.
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: page_rect,
            alpha: 1.0,
            mask: Some(empty_mask),
        });
        dl.push(RenderCommand::DrawGlyphRun(zpdf_display_list::GlyphRun {
            font_id: fid,
            font_size: 24.0,
            glyphs: vec![zpdf_display_list::PositionedGlyph {
                glyph_id: 0,
                code_len: 1,
                code: b'H' as u32,
                x: 0.0,
                y: 0.0,
                advance: 16.0,
            }],
            paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::black()),
            alpha: 1.0,
            overprint: None,
            transform: zpdf_core::Matrix::translate(80.0, 40.0),
            h_scale: 1.0,
        }));
        dl.push(RenderCommand::PopBlendGroup);

        let slide = display_list_to_slide(
            &dl,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("empty Alpha mask group with outline-less text must be skipped cleanly");
        assert_eq!(
            slide.elements.len(),
            1,
            "only the visible sibling rectangle should survive: {:?}",
            slide.elements
        );
        assert!(matches!(
            slide.elements[0].base(),
            SlideElement::Rectangle {
                fill_rgb: Some((0, 0, 255)),
                ..
            }
        ));
    }

    #[test]
    fn isolated_form_preserves_source_order_for_overlapping_black_and_red_native_text_after_overprint(
    ) {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Opaque isolated Normal Form draws cyan rect + yellow overprint rect (forcing a
        // raster image), then draws two overlapping ordinary Helvetica runs in black and red.
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 320 140] /Contents 4 0 R \
              /Resources << /XObject << /Fm1 5 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"/Fm1 Do"),                        // 4
            stream(
                "/Type /XObject /Subtype /Form /BBox [0 0 320 140] \
                 /Group << /Type /Group /S /Transparency /I true /K false >> \
                 /Resources << /Font << /FH 6 0 R >> /ExtGState << /GSop 7 0 R >> >>",
                b"0 1 1 rg 10 10 200 100 re f \
                  q /GSop gs 0 0 1 0 k 10 10 200 100 re f Q \
                  0 0 0 rg BT /FH 24 Tf 30 50 Td (Shadow) Tj ET \
                  1 0 0 rg BT /FH 24 Tf 32 52 Td (Front) Tj ET",
            ), // 5
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(), // 6
            b"<< /Type /ExtGState /op true /OPM 1 >>".to_vec(), // 7
        ]);

        let provider = std::sync::Arc::new(zpdf_font::FontProvider::memory_only());
        let doc = zpdf_document::PdfDocument::open(pdf)
            .expect("open pdf")
            .with_font_provider(provider);
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
            .expect("overlapping black and red native runs at end of Form must preserve order");
        assert_eq!(slide.elements.len(), 3, "{:?}", slide.elements);
        assert!(matches!(
            slide.elements[0].base(),
            SlideElement::Image { .. }
        ));
        assert!(matches!(
            slide.elements[1].base(),
            SlideElement::TextBox {
                text,
                color_rgb: (0, 0, 0),
                ..
            } if text == "Shadow"
        ));
        assert!(matches!(
            slide.elements[2].base(),
            SlideElement::TextBox {
                text,
                color_rgb: (255, 0, 0),
                ..
            } if text == "Front"
        ));
    }

    #[test]
    fn disjoint_metrics_only_helvetica_labels_avoid_quadratic_record_scans() {
        let page_rect = Rect::new(0.0, 0.0, 800.0, 800.0);
        let mut fonts = FontCache::new();
        let fid = fonts.insert(
            "FH".into(),
            zpdf_font::LoadedFont::new_placeholder("Helvetica".into()),
        );

        let build_labels = |with_trailing_overprint: bool| -> DisplayList {
            let mut dl = DisplayList::new(page_rect);
            // 20 x 20 = 400 disjoint labels on a 40pt grid (zero overlap).
            for row in 0..20 {
                for col in 0..20 {
                    dl.push(RenderCommand::DrawGlyphRun(zpdf_display_list::GlyphRun {
                        font_id: fid,
                        font_size: 10.0,
                        glyphs: vec![zpdf_display_list::PositionedGlyph {
                            glyph_id: 0,
                            code_len: 1,
                            code: b'X' as u32,
                            x: 0.0,
                            y: 0.0,
                            advance: 10.0,
                        }],
                        paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::black()),
                        alpha: 1.0,
                        overprint: None,
                        transform: zpdf_core::Matrix::translate(
                            col as f64 * 40.0 + 4.0,
                            row as f64 * 40.0 + 10.0,
                        ),
                        h_scale: 1.0,
                    }));
                }
            }
            if with_trailing_overprint {
                let mut r = zpdf_display_list::Path::new();
                r.rect(Rect::new(0.0, 0.0, 30.0, 30.0));
                dl.push(RenderCommand::FillPath {
                    path: r,
                    rule: zpdf_display_list::FillRule::NonZero,
                    paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::rgb(
                        1.0, 1.0, 0.0,
                    )),
                    alpha: 1.0,
                    overprint: Some(zpdf_display_list::Overprint {
                        cmyk: [0.0, 0.0, 1.0, 0.0],
                        active: zpdf_display_list::Overprint::Y,
                    }),
                });
            }
            dl
        };

        // 1. Ordinary page with no overprint or non-Normal blend: zero native-text record visits.
        let dl_plain = build_labels(false);
        let (slide_plain, stats_plain) = display_list_to_slide_internal(
            &dl_plain,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
            Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
            None,
        )
        .expect("plain labels");
        assert_eq!(slide_plain.elements.len(), 400);
        assert_eq!(
            stats_plain.native_text_record_visits, 0,
            "ordinary page with no overprint/blend must do zero native-text record scans"
        );

        // 2. Same 400 disjoint labels + 1 trailing yellow overprint: spatial indexing keeps
        //    visits linear (a tiny fraction of 400*399/2 = 79,800 quadratic visits).
        let dl_ovp = build_labels(true);
        let (slide_ovp, stats_ovp) = display_list_to_slide_internal(
            &dl_ovp,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
            Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
            None,
        )
        .expect("labels + trailing overprint");
        assert_eq!(slide_ovp.elements.len(), 401);
        assert!(
            stats_ovp.native_text_record_visits < 2_000,
            "indexed native-text record visits must stay linear across 400 disjoint labels, got {}",
            stats_ovp.native_text_record_visits
        );
    }

    #[test]
    fn many_gray_overprinting_helvetica_labels_reuse_backdrop_linearly() {
        let mut fonts = FontCache::new();
        let fid = fonts.insert(
            "FH".into(),
            zpdf_font::LoadedFont::new_placeholder("Helvetica".into()),
        );
        let op_half_gray = Some(zpdf_display_list::Overprint {
            cmyk: [0.0, 0.0, 0.0, 0.5],
            active: zpdf_display_list::Overprint::K,
        });

        let make_run = |x: f64, y: f64| {
            RenderCommand::DrawGlyphRun(zpdf_display_list::GlyphRun {
                font_id: fid,
                font_size: 10.0,
                glyphs: vec![zpdf_display_list::PositionedGlyph {
                    glyph_id: 0,
                    code_len: 1,
                    code: b'G' as u32,
                    x: 0.0,
                    y: 0.0,
                    advance: 10.0,
                }],
                paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::gray(0.5)),
                alpha: 1.0,
                overprint: op_half_gray,
                transform: zpdf_core::Matrix::translate(x, y),
                h_scale: 1.0,
            })
        };

        // 1. Standard page (600x600pt): 1 cyan rect + 200 disjoint 0.5g overprinting labels.
        //    Reused full-page backdrop executes the cyan rect ONCE across all 200 labels.
        let std_rect = Rect::new(0.0, 0.0, 600.0, 600.0);
        let mut dl_std = DisplayList::new(std_rect);
        let mut bg = zpdf_display_list::Path::new();
        bg.rect(std_rect);
        dl_std.push(RenderCommand::FillPath {
            path: bg.clone(),
            rule: zpdf_display_list::FillRule::NonZero,
            paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::rgb(0.0, 1.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        for i in 0..200 {
            dl_std.push(make_run(
                (i % 20) as f64 * 28.0 + 4.0,
                (i / 20) as f64 * 28.0 + 10.0,
            ));
        }
        let (slide_std, stats_std) = display_list_to_slide_internal(
            &dl_std,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
            Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
            None,
        )
        .expect("standard page overprint labels");
        assert_eq!(slide_std.elements.len(), 201);
        for elem in &slide_std.elements[1..] {
            assert!(matches!(
                elem.base(),
                SlideElement::TextBox {
                    color_rgb: (0, 128, 128),
                    ..
                }
            ));
        }
        assert_eq!(
            stats_std.backdrop_command_visits, 1,
            "standard page must reuse incremental backdrop and execute cyan rect once, got {}",
            stats_std.backdrop_command_visits
        );

        // 2. Isolated Normal Form (600x600pt): 1 cyan rect + 200 disjoint 0.5g overprinting labels.
        let mut dl_iso = DisplayList::new(std_rect);
        dl_iso.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: std_rect,
            alpha: 1.0,
            mask: None,
        });
        dl_iso.push(RenderCommand::FillPath {
            path: bg,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::rgb(0.0, 1.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        for i in 0..200 {
            dl_iso.push(make_run(
                (i % 20) as f64 * 28.0 + 4.0,
                (i / 20) as f64 * 28.0 + 10.0,
            ));
        }
        dl_iso.push(RenderCommand::PopBlendGroup);
        let (slide_iso, stats_iso) = display_list_to_slide_internal(
            &dl_iso,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
            Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
            None,
        )
        .expect("isolated form overprint labels");
        assert_eq!(slide_iso.elements.len(), 201);
        assert_eq!(
            stats_iso.backdrop_command_visits, 1,
            "isolated Form must reuse its incremental backdrop and execute cyan rect once, got {}",
            stats_iso.backdrop_command_visits
        );

        // 3. Large 4000x4000pt sheet: 1 cyan rect + 200 disjoint 0.5g overprinting labels.
        //    BackdropIndex restricts replay to the cyan rect only (<= 200 visits vs 20,000).
        let big_rect = Rect::new(0.0, 0.0, 4000.0, 4000.0);
        let mut dl_big = DisplayList::new(big_rect);
        let mut big_bg = zpdf_display_list::Path::new();
        big_bg.rect(big_rect);
        dl_big.push(RenderCommand::FillPath {
            path: big_bg,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::rgb(0.0, 1.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        for i in 0..200 {
            dl_big.push(make_run(
                (i % 20) as f64 * 180.0 + 20.0,
                (i / 20) as f64 * 180.0 + 20.0,
            ));
        }
        let (slide_big, stats_big) = display_list_to_slide_internal(
            &dl_big,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
            Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
            None,
        )
        .expect("large sheet overprint labels");
        assert_eq!(slide_big.elements.len(), 201);
        for elem in &slide_big.elements[1..] {
            assert!(matches!(
                elem.base(),
                SlideElement::TextBox {
                    color_rgb: (0, 128, 128),
                    ..
                }
            ));
        }
        assert!(
            stats_big.backdrop_command_visits <= 200,
            "large sheet BackdropIndex replay must visit only the cyan rect (<= 200), got {}",
            stats_big.backdrop_command_visits
        );
    }

    #[test]
    fn unmapped_type3_nested_outline_less_helvetica_in_flattened_form_matches_page_behavior() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        let run_form = |form_ops: &[u8]| {
            // Opaque isolated Normal Form painting an overprinted path (forcing the Form
            // to flatten) followed by an unmapped Type 3 icon drawing outline-less Helvetica.
            let pdf = assemble(&[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 120] /Contents 4 0 R \
                  /Resources << /XObject << /Fm1 5 0 R >> >> >>"
                    .to_vec(), // 3
                stream("", b"/Fm1 Do"),                        // 4
                stream(
                    "/Type /XObject /Subtype /Form /BBox [0 0 240 120] \
                     /Group << /Type /Group /S /Transparency /I true /K false >> \
                     /Resources << /Font << /FT3 6 0 R >> /ExtGState << /GSop 11 0 R >> >>",
                    form_ops,
                ), // 5
                b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
                  /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 7 0 R /Encoding 8 0 R \
                  /FirstChar 65 /LastChar 65 /Widths [1000] \
                  /Resources << /Font << /FH 10 0 R >> >> >>"
                    .to_vec(), // 6
                b"<< /icon 9 0 R >>".to_vec(),                 // 7
                b"<< /Type /Encoding /Differences [65 /icon] >>".to_vec(), // 8
                stream("", b"1000 0 d0 BT /FH 800 Tf 0 0 Td (A) Tj ET"), // 9
                b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(), // 10
                b"<< /Type /ExtGState /op true /OPM 1 >>".to_vec(), // 11
            ]);
            let provider = std::sync::Arc::new(zpdf_font::FontProvider::memory_only());
            let doc = zpdf_document::PdfDocument::open(pdf)
                .expect("open pdf")
                .with_font_provider(provider);
            let page = doc.page(0).expect("page 0");
            let mut fonts = doc.load_page_fonts(&page);
            let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
            let mut images = ImageCache::new();
            let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
                .with_document(doc.file(), &page.resources)
                .with_fonts(&mut fonts)
                .with_images(&mut images)
                .interpret_with_stats(&content_bytes);
            display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
        };

        // 1. Visible icon inside flattened isolated Form must report missing outlines
        //    rather than leaking its internal Helvetica "A" as an editable slide TextBox.
        let visible = run_form(
            b"0 1 1 rg 10 10 40 40 re f \
              q /GSop gs 0 0 1 0 k 10 10 40 40 re f Q \
              BT /FT3 60 Tf 80 20 Td (A) Tj ET",
        );
        assert!(
            visible.is_err(),
            "visible unmapped Type 3 icon inside flattened Form must error rather than leak a TextBox"
        );

        // 2. Disjointly clipped icon inside the same flattened Form skips cleanly and
        //    emits only the flattened Form image (no TextBox).
        let clipped = run_form(
            b"0 1 1 rg 10 10 40 40 re f \
              q /GSop gs 0 0 1 0 k 10 10 40 40 re f Q \
              q 180 20 40 40 re W n BT /FT3 60 Tf 80 20 Td (A) Tj ET Q",
        )
        .expect("clipped unmapped Type 3 icon inside flattened Form must skip cleanly");
        assert!(
            clipped
                .elements
                .iter()
                .all(|e| !matches!(e.base(), SlideElement::TextBox { .. })),
            "clipped Type 3 icon must not emit any TextBox: {:?}",
            clipped.elements
        );
        assert_eq!(clipped.elements.len(), 1);
    }

    #[test]
    fn disjointly_clipped_overprint_native_text_skips_before_backdrop_sample() {
        let page_rect = Rect::new(0.0, 0.0, 240.0, 120.0);
        let mut fonts = FontCache::new();
        let fid = fonts.insert(
            "FH".into(),
            zpdf_font::LoadedFont::new_placeholder("Helvetica".into()),
        );
        let make_clipped_dl = |cyan_width: f64| -> DisplayList {
            let mut dl = DisplayList::new(page_rect);
            let mut bg = zpdf_display_list::Path::new();
            bg.rect(Rect::new(0.0, 0.0, cyan_width, 120.0));
            dl.push(RenderCommand::FillPath {
                path: bg,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::rgb(
                    0.0, 1.0, 1.0,
                )),
                alpha: 1.0,
                overprint: None,
            });
            // Rectangular caller clip at x in [180..220], completely disjoint from label at [20..80].
            let mut clip_path = zpdf_display_list::Path::new();
            clip_path.rect(Rect::new(180.0, 10.0, 220.0, 110.0));
            dl.push(RenderCommand::PushClip {
                path: clip_path,
                rule: zpdf_display_list::FillRule::NonZero,
            });
            dl.push(RenderCommand::DrawGlyphRun(zpdf_display_list::GlyphRun {
                font_id: fid,
                font_size: 24.0,
                glyphs: vec![
                    zpdf_display_list::PositionedGlyph {
                        glyph_id: 0,
                        code_len: 1,
                        code: b'H' as u32,
                        x: 0.0,
                        y: 0.0,
                        advance: 30.0,
                    },
                    zpdf_display_list::PositionedGlyph {
                        glyph_id: 0,
                        code_len: 1,
                        code: b'i' as u32,
                        x: 30.0,
                        y: 0.0,
                        advance: 30.0,
                    },
                ],
                paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::gray(0.5)),
                alpha: 1.0,
                overprint: Some(zpdf_display_list::Overprint {
                    cmyk: [0.0, 0.0, 0.0, 0.5],
                    active: zpdf_display_list::Overprint::K,
                }),
                transform: zpdf_core::Matrix::translate(20.0, 40.0),
                h_scale: 1.0,
            }));
            dl.push(RenderCommand::PopClip);
            dl
        };

        // 1. Clipped label straddles cyan [0..50] and white [50..240]: must NOT error on
        //    non-uniform backdrop and must emit only the cyan rectangle.
        let slide_split = display_list_to_slide(
            &make_clipped_dl(50.0),
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("disjointly clipped overprint text over split backdrop must skip cleanly");
        assert_eq!(slide_split.elements.len(), 1);
        assert!(
            slide_split
                .elements
                .iter()
                .all(|e| !matches!(e.base(), SlideElement::TextBox { .. })),
            "clipped overprint text must not emit a TextBox: {:?}",
            slide_split.elements
        );

        // 2. Clipped label over a uniform cyan background [0..240]: must ALSO emit no TextBox.
        let slide_uniform = display_list_to_slide(
            &make_clipped_dl(240.0),
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("disjointly clipped overprint text over uniform backdrop must skip cleanly");
        assert_eq!(slide_uniform.elements.len(), 1);
        assert!(
            slide_uniform
                .elements
                .iter()
                .all(|e| !matches!(e.base(), SlideElement::TextBox { .. })),
            "clipped overprint text must not emit a hidden TextBox: {:?}",
            slide_uniform.elements
        );
    }

    #[test]
    fn alpha_soft_mask_with_outline_less_text_errors_when_group_visible_and_skips_when_empty() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        let run_case = |mask_ops: &[u8], masked_ops: &[u8]| {
            let pdf = assemble(&[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 120] /Contents 4 0 R \
                  /Resources << /ExtGState << /GSsm 5 0 R >> >> >>"
                    .to_vec(), // 3
                stream("", masked_ops),                        // 4
                b"<< /Type /ExtGState /SMask << /Type /Mask /S /Alpha /G 6 0 R >> >>".to_vec(), // 5
                stream(
                    "/Type /XObject /Subtype /Form /BBox [0 0 240 120] \
                     /Group << /Type /Group /S /Transparency /CS /DeviceRGB /I true >> \
                     /Resources << /Font << /FH 7 0 R >> >>",
                    mask_ops,
                ), // 6
                b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(), // 7
            ]);
            let provider = std::sync::Arc::new(zpdf_font::FontProvider::memory_only());
            let doc = zpdf_document::PdfDocument::open(pdf)
                .expect("open pdf")
                .with_font_provider(provider);
            let page = doc.page(0).expect("page 0");
            let mut fonts = doc.load_page_fonts(&page);
            let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
            let mut images = ImageCache::new();
            let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
                .with_document(doc.file(), &page.resources)
                .with_fonts(&mut fonts)
                .with_images(&mut images)
                .interpret_with_stats(&content_bytes);
            display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
        };

        // 1. `/S /Alpha` mask paints outline-less Helvetica "A" over a visible red fill:
        //    must NOT treat the mask as Empty and silently drop the red fill.
        let masked_red = run_case(
            b"0 g BT /FH 60 Tf 20 20 Td (A) Tj ET",
            b"q /GSsm gs 1 0 0 rg 0 0 240 120 re f Q",
        );
        assert!(
            masked_red.is_err(),
            "alpha soft mask with outline-less glyph over visible fill must report missing outlines"
        );

        // 2. Same mask over an empty masked group has no paint to affect: skips cleanly.
        let masked_empty = run_case(b"0 g BT /FH 60 Tf 20 20 Td (A) Tj ET", b"q /GSsm gs Q")
            .expect("missing-outline mask over empty group should skip cleanly");
        assert!(masked_empty.elements.is_empty());

        // 3. Mask whose outline-less Helvetica "A" is clipped out inside the mask IS empty
        //    and legitimately hides the red fill.
        let clipped_mask = run_case(
            b"q 180 20 40 40 re W n 0 g BT /FH 60 Tf 20 20 Td (A) Tj ET Q",
            b"q /GSsm gs 1 0 0 rg 0 0 240 120 re f Q",
        )
        .expect("alpha mask with clipped-out outline-less glyph is genuinely empty");
        assert!(clipped_mask.elements.is_empty());
    }

    #[test]
    fn promoted_overprint_text_inside_nested_isolated_form_resolves_in_child_scope() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        let run_nested = |child_ops: &[u8]| {
            // Outer opaque isolated Normal Form paints a cyan rect + yellow-only overprinted
            // path (forcing outer Form to flatten) and invokes an isolated Normal child Form.
            let pdf = assemble(&[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 120] /Contents 4 0 R \
                  /Resources << /XObject << /FmOuter 5 0 R >> >> >>"
                    .to_vec(), // 3
                stream("", b"/FmOuter Do"),                    // 4
                stream(
                    "/Type /XObject /Subtype /Form /BBox [0 0 240 120] \
                     /Group << /Type /Group /S /Transparency /I true /K false >> \
                     /Resources << /XObject << /FmChild 6 0 R >> /ExtGState << /GSop 8 0 R >> >>",
                    b"0 1 1 rg 0 0 240 120 re f \
                      q /GSop gs 0 0 1 0 k 10 10 40 40 re f Q \
                      /FmChild Do",
                ), // 5
                stream(
                    "/Type /XObject /Subtype /Form /BBox [0 0 240 120] \
                     /Group << /Type /Group /S /Transparency /I true /K false >> \
                     /Resources << /Font << /FH 7 0 R >> /ExtGState << /GSop 8 0 R >> >>",
                    child_ops,
                ), // 6
                b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(), // 7
                b"<< /Type /ExtGState /op true /OPM 1 >>".to_vec(), // 8
            ]);
            let provider = std::sync::Arc::new(zpdf_font::FontProvider::memory_only());
            let doc = zpdf_document::PdfDocument::open(pdf)
                .expect("open pdf")
                .with_font_provider(provider);
            let page = doc.page(0).expect("page 0");
            let mut fonts = doc.load_page_fonts(&page);
            let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
            let mut images = ImageCache::new();
            let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
                .with_document(doc.file(), &page.resources)
                .with_fonts(&mut fonts)
                .with_images(&mut images)
                .interpret_with_stats(&content_bytes);
            display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
        };

        // 1. Child isolated Form contains ONLY a gray 0.5g K-only overprinting outline-less
        //    Helvetica label: must succeed (no "unclosed non-passthrough group" error) and
        //    resolve against child Form's transparent backdrop => gray (128, 128, 128).
        let slide_transparent_child =
            run_nested(b"q /GSop gs 0.5 g BT /FH 24 Tf 80 40 Td (Child) Tj ET Q")
                .expect("promoted overprint text inside child isolated Form must resolve cleanly");
        assert_eq!(slide_transparent_child.elements.len(), 2);
        assert!(matches!(
            slide_transparent_child.elements[0].base(),
            SlideElement::Image { .. }
        ));
        assert!(matches!(
            slide_transparent_child.elements[1].base(),
            SlideElement::TextBox {
                text,
                color_rgb: (128, 128, 128),
                ..
            } if text == "Child"
        ));

        // 2. Child isolated Form paints its own cyan rect before the same label: must
        //    resolve against child Form's cyan backdrop => dark cyan (0, 128, 128).
        let slide_cyan_child = run_nested(
            b"0 1 1 rg 70 20 150 80 re f \
              q /GSop gs 0.5 g BT /FH 24 Tf 80 40 Td (Child) Tj ET Q",
        )
        .expect("promoted overprint text over child Form's own cyan rect must resolve cleanly");
        assert_eq!(slide_cyan_child.elements.len(), 2);
        assert!(matches!(
            slide_cyan_child.elements[1].base(),
            SlideElement::TextBox {
                text,
                color_rgb: (0, 128, 128),
                ..
            } if text == "Child"
        ));
    }

    #[test]
    fn uniform_translucent_form_backdrop_allows_representable_overprint_text() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Isolated Normal Form: 50%-opaque yellow rect + opaque Y-only overprinting
        // outline-less Helvetica label of the same yellow entirely inside it.
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 120] /Contents 4 0 R \
              /Resources << /XObject << /Fm1 5 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"/Fm1 Do"),                        // 4
            stream(
                "/Type /XObject /Subtype /Form /BBox [0 0 240 120] \
                 /Group << /Type /Group /S /Transparency /I true /K false >> \
                 /Resources << /Font << /FH 6 0 R >> \
                 /ExtGState << /GShalf 7 0 R /GSop 8 0 R >> >>",
                b"q /GShalf gs 1 1 0 rg 10 10 220 100 re f Q \
                  q /GSop gs 0 0 1 0 k BT /FH 24 Tf 30 40 Td (Yellow) Tj ET Q",
            ), // 5
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(), // 6
            b"<< /Type /ExtGState /ca 0.5 >>".to_vec(),    // 7
            b"<< /Type /ExtGState /op true /OPM 1 >>".to_vec(), // 8
        ]);
        let provider = std::sync::Arc::new(zpdf_font::FontProvider::memory_only());
        let doc = zpdf_document::PdfDocument::open(pdf)
            .expect("open pdf")
            .with_font_provider(provider);
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content_bytes);

        let slide = display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
            .expect("opaque Y-only overprint over uniform 50%-opaque yellow must succeed");
        assert_eq!(slide.elements.len(), 2);
        assert!(matches!(
            slide.elements[1],
            SlideElement::TextBox {
                ref text,
                color_rgb: (255, 255, 0),
                ..
            } if text == "Yellow"
        ));

        // Unrepresentable translucent overprint over a 90%-opaque 50% gray backdrop
        // (simultaneous C+K overprint at alpha=0.5 requires negative SourceOver red channel)
        // must still be rejected.
        let unrep_op = zpdf_display_list::Overprint {
            cmyk: [1.0, 0.0, 0.0, 1.0],
            active: zpdf_display_list::Overprint::C | zpdf_display_list::Overprint::K,
        };
        let ba = 230u8; // ~0.902
        let premul_gray = [115, 115, 115, ba];
        assert!(
            resolve_overprint_on_premul_rgba(premul_gray, &unrep_op, 0.5).is_err(),
            "out-of-gamut translucent overprint on translucent backdrop must still error"
        );
    }

    #[test]
    fn backdrop_index_filters_distant_global_rules_by_bounds_before_replay() {
        let mut fonts = FontCache::new();
        let fid = fonts.insert(
            "FH".into(),
            zpdf_font::LoadedFont::new_placeholder("Helvetica".into()),
        );
        let op_half_gray = Some(zpdf_display_list::Overprint {
            cmyk: [0.0, 0.0, 0.0, 0.5],
            active: zpdf_display_list::Overprint::K,
        });

        let sheet = Rect::new(0.0, 0.0, 10_000.0, 4_000.0);
        let build_sheet = |in_isolated_form: bool| -> DisplayList {
            let mut dl = DisplayList::new(sheet);
            if in_isolated_form {
                dl.push(RenderCommand::PushBlendGroup {
                    blend_mode: zpdf_display_list::BlendMode::Normal,
                    isolated: true,
                    knockout: false,
                    bounds: sheet,
                    alpha: 1.0,
                    mask: None,
                });
            }
            // 1 uniform cyan background across the entire sheet.
            let mut bg = zpdf_display_list::Path::new();
            bg.rect(sheet);
            dl.push(RenderCommand::FillPath {
                path: bg,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::rgb(
                    0.0, 1.0, 1.0,
                )),
                alpha: 1.0,
                overprint: None,
            });
            // 100 long horizontal rules confined to the top of the sheet (y in [3800..3990]):
            // each spans >16 512pt grid cells, so BackdropIndex places them in `global_items`.
            for i in 0..100 {
                let y = 3800.0 + i as f64 * 1.5;
                let mut rule = zpdf_display_list::Path::new();
                rule.rect(Rect::new(10.0, y, 9_990.0, y + 0.5));
                dl.push(RenderCommand::FillPath {
                    path: rule,
                    rule: zpdf_display_list::FillRule::NonZero,
                    paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::rgb(
                        1.0, 0.0, 0.0,
                    )),
                    alpha: 1.0,
                    overprint: None,
                });
            }
            // 100 disjoint 0.5g overprinting Helvetica labels along the bottom (y = 40).
            for i in 0..100 {
                dl.push(RenderCommand::DrawGlyphRun(zpdf_display_list::GlyphRun {
                    font_id: fid,
                    font_size: 10.0,
                    glyphs: vec![zpdf_display_list::PositionedGlyph {
                        glyph_id: 0,
                        code_len: 1,
                        code: b'L' as u32,
                        x: 0.0,
                        y: 0.0,
                        advance: 10.0,
                    }],
                    paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::gray(0.5)),
                    alpha: 1.0,
                    overprint: op_half_gray,
                    transform: zpdf_core::Matrix::translate(50.0 + i as f64 * 80.0, 40.0),
                    h_scale: 1.0,
                }));
            }
            if in_isolated_form {
                dl.push(RenderCommand::PopBlendGroup);
            }
            dl
        };

        for in_isolated_form in [false, true] {
            let dl = build_sheet(in_isolated_form);
            let (slide, stats) = display_list_to_slide_internal(
                &dl,
                0,
                &fonts,
                &ImageCache::new(),
                &ConversionOptions::default(),
                Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
                None,
            )
            .expect("large sheet with distant global rules and bottom labels");
            assert_eq!(slide.elements.len(), 201);
            for elem in &slide.elements[101..] {
                assert!(matches!(
                    elem.base(),
                    SlideElement::TextBox {
                        color_rgb: (0, 128, 128),
                        ..
                    }
                ));
            }
            assert_eq!(
                stats.backdrop_command_visits, 100,
                "in_isolated_form={in_isolated_form}: distant global rules must be filtered by bounds before replay (expected 100 cyan visits, got {})",
                stats.backdrop_command_visits
            );
        }
    }

    #[test]
    fn translucent_earlier_native_text_composites_with_backdrop_before_overprint_check() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        let run_case = |first_text_color: &[u8]| {
            let mut ops = b"1 0 0 rg 0 0 240 120 re f q /GShalf gs ".to_vec();
            ops.extend_from_slice(first_text_color);
            ops.extend_from_slice(
                b" BT /FH 48 Tf 40 30 Td (A) Tj ET Q \
                  q /GSop gs 0.5 0.5 0 0 k BT /FH 48 Tf 40 30 Td (A) Tj ET Q",
            );
            let pdf = assemble(&[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 120] /Contents 4 0 R \
                  /Resources << /Font << /FH 5 0 R >> \
                  /ExtGState << /GShalf 6 0 R /GSop 7 0 R >> >> >>"
                    .to_vec(), // 3
                stream("", &ops),                              // 4
                b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(), // 5
                b"<< /Type /ExtGState /ca 0.5 >>".to_vec(),    // 6
                b"<< /Type /ExtGState /op true /OPM 1 >>".to_vec(), // 7
            ]);
            let provider = std::sync::Arc::new(zpdf_font::FontProvider::memory_only());
            let doc = zpdf_document::PdfDocument::open(pdf)
                .expect("open pdf")
                .with_font_provider(provider);
            let page = doc.page(0).expect("page 0");
            let mut fonts = doc.load_page_fonts(&page);
            let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
            let mut images = ImageCache::new();
            let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
                .with_document(doc.file(), &page.resources)
                .with_fonts(&mut fonts)
                .with_images(&mut images)
                .interpret_with_stats(&content_bytes);
            display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
        };

        // 1. 50%-opaque green 'A' on opaque red composites to ~(128, 128, 0) with K~0.5,
        //    so 50%C+50%M overprint yields (64, 64, 0) inside the glyph vs (128, 128, 0)
        //    on pure red: must reject rather than accepting (128, 128, 0).
        let green_over_red = run_case(b"0 1 0 rg");
        assert!(
            green_over_red.is_err(),
            "50%-opaque green text composited on red changes overprint result to (64, 64, 0) and must error"
        );

        // 2. 50%-opaque red 'A' on opaque red composites identically to pure red (255, 0, 0),
        //    so 50%C+50%M overprint resolves uniformly to (128, 128, 0) everywhere.
        let red_over_red = run_case(b"1 0 0 rg")
            .expect("50%-opaque red on red composites identically to red and must succeed");
        assert!(matches!(
            red_over_red.elements.last().map(|e| e.base()),
            Some(SlideElement::TextBox {
                text,
                color_rgb: (128, 128, 0),
                ..
            }) if text == "A"
        ));
    }

    #[test]
    fn partially_clipped_overprint_native_text_errors_while_containing_clip_succeeds() {
        let page_rect = Rect::new(0.0, 0.0, 240.0, 120.0);
        let mut fonts = FontCache::new();
        let fid = fonts.insert(
            "FH".into(),
            zpdf_font::LoadedFont::new_placeholder("Helvetica".into()),
        );
        let make_dl = |clip_path: zpdf_display_list::Path| -> DisplayList {
            let mut dl = DisplayList::new(page_rect);
            let mut bg = zpdf_display_list::Path::new();
            bg.rect(page_rect);
            dl.push(RenderCommand::FillPath {
                path: bg,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::rgb(
                    0.0, 1.0, 1.0,
                )),
                alpha: 1.0,
                overprint: None,
            });
            dl.push(RenderCommand::PushClip {
                path: clip_path,
                rule: zpdf_display_list::FillRule::NonZero,
            });
            // "AB": 'A' spans x in [20..50], 'B' spans x in [50..80].
            dl.push(RenderCommand::DrawGlyphRun(zpdf_display_list::GlyphRun {
                font_id: fid,
                font_size: 24.0,
                glyphs: vec![
                    zpdf_display_list::PositionedGlyph {
                        glyph_id: 0,
                        code_len: 1,
                        code: b'A' as u32,
                        x: 0.0,
                        y: 0.0,
                        advance: 30.0,
                    },
                    zpdf_display_list::PositionedGlyph {
                        glyph_id: 0,
                        code_len: 1,
                        code: b'B' as u32,
                        x: 30.0,
                        y: 0.0,
                        advance: 30.0,
                    },
                ],
                paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::gray(0.5)),
                alpha: 1.0,
                overprint: Some(zpdf_display_list::Overprint {
                    cmyk: [0.0, 0.0, 0.0, 0.5],
                    active: zpdf_display_list::Overprint::K,
                }),
                transform: zpdf_core::Matrix::translate(20.0, 40.0),
                h_scale: 1.0,
            }));
            dl.push(RenderCommand::PopClip);
            dl
        };

        // 1. Rectangular clip ending right between 'A' and 'B' [10..50]: must NOT emit an
        //    unclipped "AB" TextBox that reveals 'B'; must report missing outlines.
        let mut partial_rect = zpdf_display_list::Path::new();
        partial_rect.rect(Rect::new(10.0, 10.0, 50.0, 100.0));
        let partial_res = display_list_to_slide(
            &make_dl(partial_rect),
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        );
        assert!(
            partial_res.is_err(),
            "rectangular clip crossing overprinting outline-less 'AB' must error rather than leak 'B'"
        );

        // 2. Non-rectangular triangular clip crossing 'AB': must also error.
        let mut tri = zpdf_display_list::Path::new();
        tri.move_to(zpdf_core::Point::new(0.0, 0.0));
        tri.line_to(zpdf_core::Point::new(240.0, 0.0));
        tri.line_to(zpdf_core::Point::new(120.0, 120.0));
        tri.close();
        let tri_res = display_list_to_slide(
            &make_dl(tri),
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        );
        assert!(
            tri_res.is_err(),
            "non-rectangular clip over overprinting outline-less 'AB' must error"
        );

        // 3. Rectangular clip fully containing 'AB': succeeds with resolved dark cyan.
        let mut full_rect = zpdf_display_list::Path::new();
        full_rect.rect(page_rect);
        let full_slide = display_list_to_slide(
            &make_dl(full_rect),
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("fully containing clip must preserve overprinting native text");
        assert_eq!(full_slide.elements.len(), 2);
        assert!(matches!(
            full_slide.elements[1].base(),
            SlideElement::TextBox {
                text,
                color_rgb: (0, 128, 128),
                ..
            } if text == "AB"
        ));
    }

    #[test]
    fn alpha_mask_missing_outline_outside_source_paint_region_does_not_abort_export() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Alpha mask: opaque rectangle on the left [10..80, 10..80] AND outline-less
        // Helvetica "A" confined by a clip to the far right [180..220, 10..80].
        let mask_ops = b"0 g 10 10 70 70 re f \
                         q 180 10 40 70 re W n BT /FH 48 Tf 182 20 Td (A) Tj ET Q";

        let run_masked = |page_ops: &[u8]| {
            let pdf = assemble(&[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 120] /Contents 4 0 R \
                  /Resources << /ExtGState << /GSsm 5 0 R >> >> >>"
                    .to_vec(), // 3
                stream("", page_ops),                          // 4
                b"<< /Type /ExtGState /SMask << /Type /Mask /S /Alpha /G 6 0 R >> >>".to_vec(), // 5
                stream(
                    "/Type /XObject /Subtype /Form /BBox [0 0 240 120] \
                     /Group << /Type /Group /S /Transparency /CS /DeviceRGB /I true >> \
                     /Resources << /Font << /FH 7 0 R >> >>",
                    mask_ops,
                ), // 6
                b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(), // 7
            ]);
            let provider = std::sync::Arc::new(zpdf_font::FontProvider::memory_only());
            let doc = zpdf_document::PdfDocument::open(pdf)
                .expect("open pdf")
                .with_font_provider(provider);
            let page = doc.page(0).expect("page 0");
            let mut fonts = doc.load_page_fonts(&page);
            let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
            let mut images = ImageCache::new();
            let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
                .with_document(doc.file(), &page.resources)
                .with_fonts(&mut fonts)
                .with_images(&mut images)
                .interpret_with_stats(&content_bytes);
            display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
        };

        // 1. Source fill lies entirely inside the left opaque rectangle [20..60, 20..60]:
        //    result is fully determined and must succeed cleanly.
        let left_only = run_masked(b"q /GSsm gs 1 0 0 rg 20 20 40 40 re f Q")
            .expect("source fill disjoint from right-side missing mask glyph must succeed");
        assert_eq!(left_only.elements.len(), 1);
        assert!(matches!(
            left_only.elements[0].base(),
            SlideElement::Image { .. }
        ));

        // 2. Full-page source fill constrained by a left caller clip [10..80, 10..80]:
        //    clipped source bounds do not intersect the right-side missing mask glyph -> succeeds.
        let left_clipped = run_masked(b"q 10 10 70 70 re W n /GSsm gs 1 0 0 rg 0 0 240 120 re f Q")
            .expect("caller-clipped left fill disjoint from missing mask glyph must succeed");
        assert_eq!(left_clipped.elements.len(), 1);
        assert!(matches!(
            left_clipped.elements[0].base(),
            SlideElement::Image { .. }
        ));

        // 3. Source fill overlapping the right-side missing mask glyph [170..230, 20..60]:
        //    must reject because the missing glyph can affect the output.
        let right_overlap = run_masked(b"q /GSsm gs 1 0 0 rg 170 20 60 40 re f Q");
        assert!(
            right_overlap.is_err(),
            "source fill overlapping missing mask glyph on the right must report missing outlines"
        );
    }

    #[test]
    fn alternating_parent_and_child_isolated_form_labels_preserve_parent_backdrop() {
        let form_rect = Rect::new(0.0, 0.0, 600.0, 600.0);
        let mut fonts = FontCache::new();
        let fid = fonts.insert(
            "FH".into(),
            zpdf_font::LoadedFont::new_placeholder("Helvetica".into()),
        );
        let op_half_gray = Some(zpdf_display_list::Overprint {
            cmyk: [0.0, 0.0, 0.0, 0.5],
            active: zpdf_display_list::Overprint::K,
        });

        let mut dl = DisplayList::new(form_rect);
        // Outer opaque isolated Normal Form.
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: form_rect,
            alpha: 1.0,
            mask: None,
        });
        // Cyan background across the outer Form + small overprinting path forcing flatten.
        let mut bg = zpdf_display_list::Path::new();
        bg.rect(form_rect);
        dl.push(RenderCommand::FillPath {
            path: bg,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::rgb(0.0, 1.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        let mut ovp_rect = zpdf_display_list::Path::new();
        ovp_rect.rect(Rect::new(4.0, 4.0, 20.0, 20.0));
        dl.push(RenderCommand::FillPath {
            path: ovp_rect,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::rgb(1.0, 1.0, 0.0)),
            alpha: 1.0,
            overprint: Some(zpdf_display_list::Overprint {
                cmyk: [0.0, 0.0, 1.0, 0.0],
                active: zpdf_display_list::Overprint::Y,
            }),
        });

        // Alternate 60 gray overprinting labels in the parent Form with 60 ordinary labels
        // in child isolated Normal Forms (all mutually disjoint).
        for i in 0..60 {
            let px = (i % 12) as f64 * 48.0 + 10.0;
            let py = (i / 12) as f64 * 100.0 + 40.0;
            dl.push(RenderCommand::DrawGlyphRun(zpdf_display_list::GlyphRun {
                font_id: fid,
                font_size: 10.0,
                glyphs: vec![zpdf_display_list::PositionedGlyph {
                    glyph_id: 0,
                    code_len: 1,
                    code: b'P' as u32,
                    x: 0.0,
                    y: 0.0,
                    advance: 10.0,
                }],
                paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::gray(0.5)),
                alpha: 1.0,
                overprint: op_half_gray,
                transform: zpdf_core::Matrix::translate(px, py),
                h_scale: 1.0,
            }));

            dl.push(RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: form_rect,
                alpha: 1.0,
                mask: None,
            });
            dl.push(RenderCommand::DrawGlyphRun(zpdf_display_list::GlyphRun {
                font_id: fid,
                font_size: 10.0,
                glyphs: vec![zpdf_display_list::PositionedGlyph {
                    glyph_id: 0,
                    code_len: 1,
                    code: b'C' as u32,
                    x: 0.0,
                    y: 0.0,
                    advance: 10.0,
                }],
                paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::black()),
                alpha: 1.0,
                overprint: None,
                transform: zpdf_core::Matrix::translate(px, py + 40.0),
                h_scale: 1.0,
            }));
            dl.push(RenderCommand::PopBlendGroup);
        }
        dl.push(RenderCommand::PopBlendGroup);

        let (slide, stats) = display_list_to_slide_internal(
            &dl,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
            Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
            None,
        )
        .expect("alternating parent and child isolated Form labels");

        // 1 flattened outer Form image + 120 promoted native TextBoxes.
        assert_eq!(slide.elements.len(), 121);
        for i in 0..60 {
            assert!(matches!(
                slide.elements[1 + 2 * i].base(),
                SlideElement::TextBox {
                    text,
                    color_rgb: (0, 128, 128),
                    ..
                } if text == "P"
            ));
            assert!(matches!(
                slide.elements[1 + 2 * i + 1].base(),
                SlideElement::TextBox {
                    text,
                    color_rgb: (0, 0, 0),
                    ..
                } if text == "C"
            ));
        }
        // Without keeping the parent FullPageBackdrop across child Forms, replaying
        // from scope_body_start on every return yields ~3,600+ visits; resuming the
        // cached parent visits each command in the outer Form at most once (<= 200).
        assert!(
            stats.backdrop_command_visits <= 200,
            "resumed parent backdrop must advance monotonically in O(N) visits (<= 200), got {}",
            stats.backdrop_command_visits
        );
    }

    #[test]
    fn shared_soft_mask_caches_outline_analysis_across_repeated_uses_and_skips_zero_alpha() {
        let page_rect = Rect::new(0.0, 0.0, 240.0, 120.0);
        let fonts = FontCache::new();

        // Build one shared 50-command mask display list containing only paths.
        let mut mask_dl = DisplayList::new(page_rect);
        for i in 0..50 {
            let mut p = zpdf_display_list::Path::new();
            p.rect(Rect::new(i as f64 * 2.0, 10.0, i as f64 * 2.0 + 1.5, 40.0));
            mask_dl.push(RenderCommand::FillPath {
                path: p,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::white()),
                alpha: 1.0,
                overprint: None,
            });
        }
        let shared_mask_dl = std::sync::Arc::new(mask_dl);

        for kind in [
            zpdf_display_list::SoftMaskKind::Luminosity,
            zpdf_display_list::SoftMaskKind::Alpha,
        ] {
            let mut dl = DisplayList::new(page_rect);
            for i in 0..60 {
                let mut fill = zpdf_display_list::Path::new();
                fill.rect(Rect::new(10.0, 10.0, 30.0, 30.0));
                dl.push(RenderCommand::PushBlendGroup {
                    blend_mode: zpdf_display_list::BlendMode::Normal,
                    isolated: true,
                    knockout: false,
                    bounds: page_rect,
                    alpha: 1.0,
                    mask: Some(zpdf_display_list::SoftMask {
                        kind,
                        commands: std::sync::Arc::clone(&shared_mask_dl),
                        offset: ((i % 5) as f32, 0.0),
                        backdrop_luma: 0.0,
                        transfer: None,
                    }),
                });
                dl.push(RenderCommand::FillPath {
                    path: fill,
                    rule: zpdf_display_list::FillRule::NonZero,
                    paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::rgb(
                        1.0, 0.0, 0.0,
                    )),
                    alpha: 1.0,
                    overprint: None,
                });
                dl.push(RenderCommand::PopBlendGroup);
            }
            let (_slide, stats) = display_list_to_slide_internal(
                &dl,
                0,
                &fonts,
                &ImageCache::new(),
                &ConversionOptions::default(),
                Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
                None,
            )
            .expect("repeated shared soft mask");
            assert_eq!(
                stats.soft_mask_analysis_visits, 50,
                "kind={kind:?}: 60 uses of one shared 50-command mask must analyze the mask once (50 visits, not 3000), got {}",
                stats.soft_mask_analysis_visits
            );
        }

        // Zero-opacity groups must exit BEFORE analyzing their soft mask (0 visits).
        let mut dl_zero = DisplayList::new(page_rect);
        for _ in 0..10 {
            let mut fill = zpdf_display_list::Path::new();
            fill.rect(Rect::new(10.0, 10.0, 30.0, 30.0));
            dl_zero.push(RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: page_rect,
                alpha: 0.0,
                mask: Some(zpdf_display_list::SoftMask {
                    kind: zpdf_display_list::SoftMaskKind::Luminosity,
                    commands: std::sync::Arc::clone(&shared_mask_dl),
                    offset: (0.0, 0.0),
                    backdrop_luma: 0.0,
                    transfer: None,
                }),
            });
            dl_zero.push(RenderCommand::FillPath {
                path: fill,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::rgb(
                    1.0, 0.0, 0.0,
                )),
                alpha: 1.0,
                overprint: None,
            });
            dl_zero.push(RenderCommand::PopBlendGroup);
        }
        let (slide_zero, stats_zero) = display_list_to_slide_internal(
            &dl_zero,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
            Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
            None,
        )
        .expect("zero-opacity masked groups");
        assert!(slide_zero.elements.is_empty());
        assert_eq!(
            stats_zero.soft_mask_analysis_visits, 0,
            "zero-opacity groups must skip soft-mask analysis completely, got {}",
            stats_zero.soft_mask_analysis_visits
        );
    }

    #[test]
    fn occluded_earlier_native_text_rejects_later_overprint_color_check() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        let run_case = |cover_ops: &[u8]| {
            // Green page, opaque red outline-less Helvetica 'A', intervening green cover rect,
            // then opaque 50%C + 50%M overprinting Helvetica 'A' under /OPM 1.
            let mut ops = b"0 1 0 rg 0 0 240 120 re f \
                            1 0 0 rg BT /FH 48 Tf 40 30 Td (A) Tj ET "
                .to_vec();
            ops.extend_from_slice(cover_ops);
            ops.extend_from_slice(b" q /GSop gs 0.5 0.5 0 0 k BT /FH 48 Tf 40 30 Td (A) Tj ET Q");
            let pdf = assemble(&[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 120] /Contents 4 0 R \
                  /Resources << /Font << /FH 5 0 R >> \
                  /ExtGState << /GShalf 6 0 R /GSop 7 0 R >> >> >>"
                    .to_vec(), // 3
                stream("", &ops),                              // 4
                b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(), // 5
                b"<< /Type /ExtGState /ca 0.5 >>".to_vec(),    // 6
                b"<< /Type /ExtGState /op true /OPM 1 >>".to_vec(), // 7
            ]);
            let provider = std::sync::Arc::new(zpdf_font::FontProvider::memory_only());
            let doc = zpdf_document::PdfDocument::open(pdf)
                .expect("open pdf")
                .with_font_provider(provider);
            let page = doc.page(0).expect("page 0");
            let mut fonts = doc.load_page_fonts(&page);
            let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
            let mut images = ImageCache::new();
            let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
                .with_document(doc.file(), &page.resources)
                .with_fonts(&mut fonts)
                .with_images(&mut images)
                .interpret_with_stats(&content_bytes);
            display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
        };

        // 1. Intervening 50%-opaque green rect blends over the red 'A' to ~(128,128,0),
        //    so the later 50%C+50%M overprint should yield ~(64,64,0) instead of (128,128,0):
        //    must reject because the earlier red record is occluded by intervening paint.
        let translucent_cover = run_case(b"q /GShalf gs 0 1 0 rg 0 0 240 120 re f Q");
        assert!(
            translucent_cover.is_err(),
            "translucent intervening cover over earlier red native text must reject later overprint"
        );

        // 2. Intervening OPAQUE green rect completely buries the red 'A', retiring it:
        //    later 50%C+50%M overprint sees pure green everywhere and resolves cleanly to (128,128,0).
        let opaque_cover = run_case(b"0 1 0 rg 0 0 240 120 re f")
            .expect("opaque cover completely buries earlier red native text");
        assert!(matches!(
            opaque_cover.elements.last().map(|e| e.base()),
            Some(SlideElement::TextBox {
                text,
                color_rgb: (128, 128, 0),
                ..
            }) if text == "A"
        ));
    }

    #[test]
    fn identity_v_vertical_missing_mask_glyph_uses_vertical_geometry() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Alpha mask: opaque rect on the far left [10..40, 20..100] + 40pt Identity-V
        // CIDFontType2 glyph (DW=1000, default DW2) at x=100 (glyph cell spans x=80..120).
        let run_case = |mask_ops: &[u8], page_ops: &[u8]| {
            let pdf = assemble(&[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 120] /Contents 4 0 R \
                  /Resources << /ExtGState << /GSsm 5 0 R >> >> >>"
                    .to_vec(), // 3
                stream("", page_ops),                          // 4
                b"<< /Type /ExtGState /SMask << /Type /Mask /S /Alpha /G 6 0 R >> >>".to_vec(), // 5
                stream(
                    "/Type /XObject /Subtype /Form /BBox [0 0 240 120] \
                     /Group << /Type /Group /S /Transparency /CS /DeviceRGB /I true >> \
                     /Resources << /Font << /FV 7 0 R >> >>",
                    mask_ops,
                ), // 6
                b"<< /Type /Font /Subtype /Type0 /BaseFont /MissingVertCid \
                  /Encoding /Identity-V /DescendantFonts [8 0 R] >>"
                    .to_vec(), // 7
                b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /MissingVertCid \
                  /DW 1000 /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> >>"
                    .to_vec(), // 8
            ]);
            let provider = std::sync::Arc::new(zpdf_font::FontProvider::memory_only());
            let doc = zpdf_document::PdfDocument::open(pdf)
                .expect("open pdf")
                .with_font_provider(provider);
            let page = doc.page(0).expect("page 0");
            let mut fonts = doc.load_page_fonts(&page);
            let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
            let mut images = ImageCache::new();
            let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
                .with_document(doc.file(), &page.resources)
                .with_fonts(&mut fonts)
                .with_images(&mut images)
                .interpret_with_stats(&content_bytes);
            display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
        };

        // 1. Source fill at x in [92..108] directly under the Identity-V glyph at x=100
        //    (cell [80..120]): must report missing mask outlines instead of dropping artwork.
        let over_vertical_glyph = run_case(
            b"0 g BT /FV 40 Tf 100 100 Td <0041> Tj ET",
            b"q /GSsm gs 1 0 0 rg 92 40 16 40 re f Q",
        );
        assert!(
            over_vertical_glyph.is_err(),
            "Identity-V missing mask glyph at x=100 (cell 80..120) must overlap fill at x=92..108"
        );

        // 2. Source fill entirely inside a far-left opaque mask rect [10..40] (disjoint
        //    from the Identity-V glyph cell [80..120]): succeeds cleanly.
        let disjoint_left = run_case(
            b"0 g 10 20 30 80 re f BT /FV 40 Tf 100 100 Td <0041> Tj ET",
            b"q /GSsm gs 1 0 0 rg 15 30 20 50 re f Q",
        )
        .expect(
            "fill confined to far-left opaque mask rect disjoint from Identity-V cell must succeed",
        );
        assert_eq!(disjoint_left.elements.len(), 1);
    }

    #[test]
    fn clipped_ordinary_native_text_skips_when_disjoint_and_errors_when_partial() {
        let page_rect = Rect::new(0.0, 0.0, 240.0, 120.0);
        let mut fonts = FontCache::new();
        let fid = fonts.insert(
            "FH".into(),
            zpdf_font::LoadedFont::new_placeholder("Helvetica".into()),
        );

        // 1. Red ordinary metrics-only Helvetica 'A' entirely outside rectangular clip
        //    [160..220], followed after PopClip by K-only 50% overprinting 'A' at the
        //    same position on white: clipped red run must skip (never enter native_only_texts),
        //    so the second run resolves cleanly to gray (128, 128, 128).
        let mut dl_disjoint = DisplayList::new(page_rect);
        let mut far_clip = zpdf_display_list::Path::new();
        far_clip.rect(Rect::new(160.0, 10.0, 220.0, 110.0));
        dl_disjoint.push(RenderCommand::PushClip {
            path: far_clip,
            rule: zpdf_display_list::FillRule::NonZero,
        });
        dl_disjoint.push(RenderCommand::DrawGlyphRun(zpdf_display_list::GlyphRun {
            font_id: fid,
            font_size: 24.0,
            glyphs: vec![zpdf_display_list::PositionedGlyph {
                glyph_id: 0,
                code_len: 1,
                code: b'A' as u32,
                x: 0.0,
                y: 0.0,
                advance: 30.0,
            }],
            paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
            transform: zpdf_core::Matrix::translate(20.0, 40.0),
            h_scale: 1.0,
        }));
        dl_disjoint.push(RenderCommand::PopClip);
        dl_disjoint.push(RenderCommand::DrawGlyphRun(zpdf_display_list::GlyphRun {
            font_id: fid,
            font_size: 24.0,
            glyphs: vec![zpdf_display_list::PositionedGlyph {
                glyph_id: 0,
                code_len: 1,
                code: b'A' as u32,
                x: 0.0,
                y: 0.0,
                advance: 30.0,
            }],
            paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::gray(0.5)),
            alpha: 1.0,
            overprint: Some(zpdf_display_list::Overprint {
                cmyk: [0.0, 0.0, 0.0, 0.5],
                active: zpdf_display_list::Overprint::K,
            }),
            transform: zpdf_core::Matrix::translate(20.0, 40.0),
            h_scale: 1.0,
        }));
        let slide = display_list_to_slide(
            &dl_disjoint,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("disjointly clipped red run must not block subsequent overprinting 'A'");
        assert_eq!(slide.elements.len(), 1);
        assert!(matches!(
            slide.elements[0].base(),
            SlideElement::TextBox {
                text,
                color_rgb: (128, 128, 128),
                ..
            } if text == "A"
        ));

        // 2. Ordinary (non-overprint) red metrics-only Helvetica "AB" partially clipped
        //    between 'A' and 'B': must report missing outlines rather than leak 'B'.
        let mut dl_partial = DisplayList::new(page_rect);
        let mut half_clip = zpdf_display_list::Path::new();
        half_clip.rect(Rect::new(10.0, 10.0, 50.0, 100.0));
        dl_partial.push(RenderCommand::PushClip {
            path: half_clip,
            rule: zpdf_display_list::FillRule::NonZero,
        });
        dl_partial.push(RenderCommand::DrawGlyphRun(zpdf_display_list::GlyphRun {
            font_id: fid,
            font_size: 24.0,
            glyphs: vec![
                zpdf_display_list::PositionedGlyph {
                    glyph_id: 0,
                    code_len: 1,
                    code: b'A' as u32,
                    x: 0.0,
                    y: 0.0,
                    advance: 30.0,
                },
                zpdf_display_list::PositionedGlyph {
                    glyph_id: 0,
                    code_len: 1,
                    code: b'B' as u32,
                    x: 30.0,
                    y: 0.0,
                    advance: 30.0,
                },
            ],
            paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
            transform: zpdf_core::Matrix::translate(20.0, 40.0),
            h_scale: 1.0,
        }));
        dl_partial.push(RenderCommand::PopClip);
        assert!(
            display_list_to_slide(
                &dl_partial,
                0,
                &fonts,
                &ImageCache::new(),
                &ConversionOptions::default(),
            )
            .is_err(),
            "partially clipped ordinary outline-less 'AB' must error rather than leak 'B'"
        );

        // 3. Standalone zero-advance combining mark (horizontal CID font with /W [1 [0]]
        //    and /ToUnicode mapping to U+0301) shown unclipped with an empty font provider:
        //    must NOT be dropped as zero-width; its Unicode mark reaches the slide TextBox.
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };
        let to_unicode = b"/CIDInit /ProcSet findresource begin\n\
            12 dict begin begincmap\n\
            /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
            /CMapName /MarkUCS def /CMapType 2 def\n\
            1 begincodespacerange <0000> <FFFF> endcodespacerange\n\
            1 beginbfchar <0001> <0301> endbfchar\n\
            endcmap CMapName currentdict /CMap defineresource pop end end";
        let mark_pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 120] /Contents 4 0 R \
              /Resources << /Font << /FC 5 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"BT /FC 24 Tf 100 60 Td <0001> Tj ET"), // 4
            b"<< /Type /Font /Subtype /Type0 /BaseFont /MissingCidMark \
              /Encoding /Identity-H /DescendantFonts [6 0 R] /ToUnicode 7 0 R >>"
                .to_vec(), // 5
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /MissingCidMark \
              /W [1 [0]] /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> >>"
                .to_vec(), // 6
            stream("", to_unicode),                        // 7
        ]);
        let provider = std::sync::Arc::new(zpdf_font::FontProvider::memory_only());
        let doc = zpdf_document::PdfDocument::open(mark_pdf)
            .expect("open mark pdf")
            .with_font_provider(provider);
        let page = doc.page(0).expect("page 0");
        let mut mark_fonts = doc.load_page_fonts(&page);
        let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let mut images = ImageCache::new();
        let (mark_dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut mark_fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content_bytes);
        let mark_slide = display_list_to_slide(
            &mark_dl,
            0,
            &mark_fonts,
            &images,
            &ConversionOptions::default(),
        )
        .expect("standalone zero-advance combining mark must export");
        assert_eq!(mark_slide.elements.len(), 1);
        assert!(matches!(
            mark_slide.elements[0].base(),
            SlideElement::TextBox { text, .. } if text == "\u{0301}"
        ));
    }

    #[test]
    fn disjoint_translucent_earlier_labels_allow_spanning_overprint_while_stacked_rejects() {
        let page_rect = Rect::new(0.0, 0.0, 240.0, 120.0);
        let mut fonts = FontCache::new();
        let fid = fonts.insert(
            "FH".into(),
            zpdf_font::LoadedFont::new_placeholder("Helvetica".into()),
        );
        let make_run =
            |x: f64, width: f64, alpha: f32, overprint: Option<zpdf_display_list::Overprint>| {
                RenderCommand::DrawGlyphRun(zpdf_display_list::GlyphRun {
                    font_id: fid,
                    font_size: 24.0,
                    glyphs: vec![zpdf_display_list::PositionedGlyph {
                        glyph_id: 0,
                        code_len: 1,
                        code: b'X' as u32,
                        x: 0.0,
                        y: 0.0,
                        advance: width as f32,
                    }],
                    paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::gray(0.5)),
                    alpha,
                    overprint,
                    transform: zpdf_core::Matrix::translate(x, 40.0),
                    h_scale: 1.0,
                })
            };
        let op_k_half = Some(zpdf_display_list::Overprint {
            cmyk: [0.0, 0.0, 0.0, 0.5],
            active: zpdf_display_list::Overprint::K,
        });

        // 1. Two separated 50%-opaque neutral-gray labels [20..50] and [100..130] on white,
        //    spanned by one opaque K-only 50% overprinting run [20..130]: succeeds cleanly!
        let mut dl_disjoint = DisplayList::new(page_rect);
        dl_disjoint.push(make_run(20.0, 30.0, 0.5, None));
        dl_disjoint.push(make_run(100.0, 30.0, 0.5, None));
        dl_disjoint.push(make_run(20.0, 110.0, 1.0, op_k_half));
        let slide = display_list_to_slide(
            &dl_disjoint,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("disjoint translucent neutral-gray labels under K-only overprint must succeed");
        assert_eq!(slide.elements.len(), 3);
        assert!(matches!(
            slide.elements[2].base(),
            SlideElement::TextBox {
                color_rgb: (128, 128, 128),
                ..
            }
        ));

        // 2. Two stacked (overlapping) 50%-opaque neutral-gray labels at [20..50] on white,
        //    under the same overprinting run: stacked composite is unresolved -> rejects!
        let mut dl_stacked = DisplayList::new(page_rect);
        dl_stacked.push(make_run(20.0, 30.0, 0.5, None));
        dl_stacked.push(make_run(25.0, 30.0, 0.5, None));
        dl_stacked.push(make_run(20.0, 110.0, 1.0, op_k_half));
        assert!(
            display_list_to_slide(
                &dl_stacked,
                0,
                &fonts,
                &ImageCache::new(),
                &ConversionOptions::default(),
            )
            .is_err(),
            "stacked overlapping translucent labels under overprinting run must error"
        );
    }

    #[test]
    fn disjoint_opposite_edge_missing_mask_glyphs_preserve_center_fill() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Alpha mask: opaque center rect [80..160, 20..100] + two outline-less Helvetica 'A'
        // glyphs clipped to opposite narrow edge strips [0..20] and [220..240].
        let mask_ops = b"0 g 80 20 80 80 re f \
                         q 0 20 20 80 re W n BT /FH 48 Tf 2 30 Td (A) Tj ET Q \
                         q 220 20 20 80 re W n BT /FH 48 Tf 222 30 Td (A) Tj ET Q";

        let run_masked = |page_ops: &[u8]| {
            let pdf = assemble(&[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 120] /Contents 4 0 R \
                  /Resources << /ExtGState << /GSsm 5 0 R >> >> >>"
                    .to_vec(), // 3
                stream("", page_ops),                          // 4
                b"<< /Type /ExtGState /SMask << /Type /Mask /S /Alpha /G 6 0 R >> >>".to_vec(), // 5
                stream(
                    "/Type /XObject /Subtype /Form /BBox [0 0 240 120] \
                     /Group << /Type /Group /S /Transparency /CS /DeviceRGB /I true >> \
                     /Resources << /Font << /FH 7 0 R >> >>",
                    mask_ops,
                ), // 6
                b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(), // 7
            ]);
            let provider = std::sync::Arc::new(zpdf_font::FontProvider::memory_only());
            let doc = zpdf_document::PdfDocument::open(pdf)
                .expect("open pdf")
                .with_font_provider(provider);
            let page = doc.page(0).expect("page 0");
            let mut fonts = doc.load_page_fonts(&page);
            let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
            let mut images = ImageCache::new();
            let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
                .with_document(doc.file(), &page.resources)
                .with_fonts(&mut fonts)
                .with_images(&mut images)
                .interpret_with_stats(&content_bytes);
            display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
        };

        // 1. Source fill entirely inside the center rect [90..150, 30..90]: lies in the
        //    gap between opposite-edge missing strips -> must export faithfully!
        let center_slide = run_masked(b"q /GSsm gs 1 0 0 rg 90 30 60 60 re f Q")
            .expect("center fill between opposite-edge missing mask strips must succeed");
        assert_eq!(center_slide.elements.len(), 1);
        assert!(matches!(
            center_slide.elements[0].base(),
            SlideElement::Image { .. }
        ));

        // 2. Source fill overlapping the left edge strip [5..30, 30..90] -> must error.
        assert!(
            run_masked(b"q /GSsm gs 1 0 0 rg 5 30 25 60 re f Q").is_err(),
            "fill overlapping left missing mask strip must error"
        );

        // 3. Source fill overlapping the right edge strip [210..235, 30..90] -> must error.
        assert!(
            run_masked(b"q /GSsm gs 1 0 0 rg 210 30 25 60 re f Q").is_err(),
            "fill overlapping right missing mask strip must error"
        );
    }

    #[test]
    fn deep_nested_rect_only_alpha_masks_succeed_and_depth_limit_never_poisons_cache() {
        let page_rect = Rect::new(0.0, 0.0, 240.0, 120.0);
        let fonts = FontCache::new();

        let make_nested_mask = |levels: usize| -> zpdf_display_list::SoftMask {
            let mut inner_dl = DisplayList::new(page_rect);
            let mut r = zpdf_display_list::Path::new();
            r.rect(Rect::new(20.0, 20.0, 100.0, 100.0));
            inner_dl.push(RenderCommand::FillPath {
                path: r.clone(),
                rule: zpdf_display_list::FillRule::NonZero,
                paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::white()),
                alpha: 1.0,
                overprint: None,
            });
            let mut cur_mask = zpdf_display_list::SoftMask {
                kind: zpdf_display_list::SoftMaskKind::Alpha,
                commands: std::sync::Arc::new(inner_dl),
                offset: (0.0, 0.0),
                backdrop_luma: 0.0,
                transfer: None,
            };
            for _ in 1..levels {
                let mut dl = DisplayList::new(page_rect);
                dl.push(RenderCommand::PushBlendGroup {
                    blend_mode: zpdf_display_list::BlendMode::Normal,
                    isolated: true,
                    knockout: false,
                    bounds: page_rect,
                    alpha: 1.0,
                    mask: Some(cur_mask),
                });
                dl.push(RenderCommand::FillPath {
                    path: r.clone(),
                    rule: zpdf_display_list::FillRule::NonZero,
                    paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::white()),
                    alpha: 1.0,
                    overprint: None,
                });
                dl.push(RenderCommand::PopBlendGroup);
                cur_mask = zpdf_display_list::SoftMask {
                    kind: zpdf_display_list::SoftMaskKind::Alpha,
                    commands: std::sync::Arc::new(dl),
                    offset: (0.0, 0.0),
                    backdrop_luma: 0.0,
                    transfer: None,
                };
            }
            cur_mask
        };

        // 1. 10 nested rect-only alpha masks (and up to 16 supported levels) contain
        //    only rectangles and must export cleanly without false missing-font errors.
        for levels in [10usize, 16usize] {
            let mut dl = DisplayList::new(page_rect);
            let mut fill = zpdf_display_list::Path::new();
            fill.rect(Rect::new(30.0, 30.0, 90.0, 90.0));
            dl.push(RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: page_rect,
                alpha: 1.0,
                mask: Some(make_nested_mask(levels)),
            });
            dl.push(RenderCommand::FillPath {
                path: fill,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::rgb(
                    1.0, 0.0, 0.0,
                )),
                alpha: 1.0,
                overprint: None,
            });
            dl.push(RenderCommand::PopBlendGroup);
            let slide = display_list_to_slide(
                &dl,
                0,
                &fonts,
                &ImageCache::new(),
                &ConversionOptions::default(),
            )
            .unwrap_or_else(|e| panic!("{levels} nested rect-only masks must succeed: {e}"));
            assert_eq!(slide.elements.len(), 1);
        }

        // 2. Depth-limited evaluation (17 levels) must NOT poison SoftMaskAnalysisCache
        //    for a shallower reuse of an inner mask on the same slide.
        let inner_2_level = make_nested_mask(2);
        let mut cur_mask = inner_2_level.clone();
        let mut r = zpdf_display_list::Path::new();
        r.rect(Rect::new(20.0, 20.0, 100.0, 100.0));
        for _ in 0..15 {
            let mut dl = DisplayList::new(page_rect);
            dl.push(RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: page_rect,
                alpha: 1.0,
                mask: Some(cur_mask),
            });
            dl.push(RenderCommand::FillPath {
                path: r.clone(),
                rule: zpdf_display_list::FillRule::NonZero,
                paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::white()),
                alpha: 1.0,
                overprint: None,
            });
            dl.push(RenderCommand::PopBlendGroup);
            cur_mask = zpdf_display_list::SoftMask {
                kind: zpdf_display_list::SoftMaskKind::Alpha,
                commands: std::sync::Arc::new(dl),
                offset: (0.0, 0.0),
                backdrop_luma: 0.0,
                transfer: None,
            };
        }
        let mut cache = SoftMaskAnalysisCache::default();
        let mut stats = ConversionStats::default();
        let deep_res = cache.analyze(Some(&cur_mask), &fonts, &mut stats);
        assert!(matches!(
            deep_res.missing_text,
            MissingTextCoverage::Unbounded
        ));
        let shallow_res = cache.analyze(Some(&inner_2_level), &fonts, &mut stats);
        assert!(
            matches!(shallow_res.missing_text, MissingTextCoverage::None),
            "depth-limited evaluation must not poison cache for shallow reuse of inner mask: {:?}",
            shallow_res.missing_text
        );
    }

    #[test]
    fn identity_v_negative_h_scale_missing_mask_glyph_covers_reflected_cell() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        let run_case = |mask_ops: &[u8], page_ops: &[u8]| {
            let pdf = assemble(&[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 120] /Contents 4 0 R \
                  /Resources << /ExtGState << /GSsm 5 0 R >> >> >>"
                    .to_vec(), // 3
                stream("", page_ops),                          // 4
                b"<< /Type /ExtGState /SMask << /Type /Mask /S /Alpha /G 6 0 R >> >>".to_vec(), // 5
                stream(
                    "/Type /XObject /Subtype /Form /BBox [0 0 240 120] \
                     /Group << /Type /Group /S /Transparency /CS /DeviceRGB /I true >> \
                     /Resources << /Font << /FV 7 0 R >> >>",
                    mask_ops,
                ), // 6
                b"<< /Type /Font /Subtype /Type0 /BaseFont /MissingVertCid \
                  /Encoding /Identity-V /DescendantFonts [8 0 R] >>"
                    .to_vec(), // 7
                b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /MissingVertCid \
                  /DW 1000 /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> >>"
                    .to_vec(), // 8
            ]);
            let provider = std::sync::Arc::new(zpdf_font::FontProvider::memory_only());
            let doc = zpdf_document::PdfDocument::open(pdf)
                .expect("open pdf")
                .with_font_provider(provider);
            let page = doc.page(0).expect("page 0");
            let mut fonts = doc.load_page_fonts(&page);
            let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
            let mut images = ImageCache::new();
            let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
                .with_document(doc.file(), &page.resources)
                .with_fonts(&mut fonts)
                .with_images(&mut images)
                .interpret_with_stats(&content_bytes);
            display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
        };

        // 1. 40pt Identity-V glyph at x=100 with `-100 Tz` reflects into x=40..80:
        //    source fill crossing x=60 ([52..68, 40..80]) must report missing mask outlines.
        let reflected_overlap = run_case(
            b"0 g BT /FV 40 Tf -100 Tz 100 100 Td <0041> Tj ET",
            b"q /GSsm gs 1 0 0 rg 52 40 16 40 re f Q",
        );
        assert!(
            reflected_overlap.is_err(),
            "Identity-V glyph with -100 Tz at x=100 reflects to x=40..80 and must overlap fill near x=60"
        );

        // 2. Source fill inside an opaque far-right mask rect [150..220] (disjoint from
        //    the reflected cell [40..80]): succeeds cleanly.
        let disjoint_right = run_case(
            b"0 g 150 20 70 80 re f BT /FV 40 Tf -100 Tz 100 100 Td <0041> Tj ET",
            b"q /GSsm gs 1 0 0 rg 160 30 50 50 re f Q",
        )
        .expect(
            "fill confined to far-right opaque mask rect disjoint from reflected cell must succeed",
        );
        assert_eq!(disjoint_right.elements.len(), 1);
    }

    #[test]
    fn partial_occlusion_on_long_native_label_allows_overprint_on_untouched_end() {
        let page_rect = Rect::new(0.0, 0.0, 240.0, 120.0);
        let mut fonts = FontCache::new();
        let fid = fonts.insert(
            "FH".into(),
            zpdf_font::LoadedFont::new_placeholder("Helvetica".into()),
        );
        let op_k_half = Some(zpdf_display_list::Overprint {
            cmyk: [0.0, 0.0, 0.0, 0.5],
            active: zpdf_display_list::Overprint::K,
        });

        let build_dl = |overprint_x: f64, blue_alpha: f32| -> DisplayList {
            let mut dl = DisplayList::new(page_rect);
            // Long neutral-gray native label spanning x in [10..210].
            dl.push(RenderCommand::DrawGlyphRun(zpdf_display_list::GlyphRun {
                font_id: fid,
                font_size: 24.0,
                glyphs: vec![zpdf_display_list::PositionedGlyph {
                    glyph_id: 0,
                    code_len: 1,
                    code: b'L' as u32,
                    x: 0.0,
                    y: 0.0,
                    advance: 200.0,
                }],
                paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::gray(0.5)),
                alpha: 1.0,
                overprint: None,
                transform: zpdf_core::Matrix::translate(10.0, 40.0),
                h_scale: 1.0,
            }));
            // Blue rectangle covering ONLY the left end [0..60] of the label.
            let mut blue = zpdf_display_list::Path::new();
            blue.rect(Rect::new(0.0, 0.0, 60.0, 120.0));
            dl.push(RenderCommand::FillPath {
                path: blue,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::rgb(
                    0.0, 0.0, 1.0,
                )),
                alpha: blue_alpha,
                overprint: None,
            });
            // 50% K-only overprinting run at `overprint_x..overprint_x+40`.
            dl.push(RenderCommand::DrawGlyphRun(zpdf_display_list::GlyphRun {
                font_id: fid,
                font_size: 24.0,
                glyphs: vec![zpdf_display_list::PositionedGlyph {
                    glyph_id: 0,
                    code_len: 1,
                    code: b'R' as u32,
                    x: 0.0,
                    y: 0.0,
                    advance: 40.0,
                }],
                paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::gray(0.5)),
                alpha: 1.0,
                overprint: op_k_half,
                transform: zpdf_core::Matrix::translate(overprint_x, 40.0),
                h_scale: 1.0,
            }));
            dl
        };

        // 1. Overprint over untouched right end [150..190] never touches the blue rect
        //    covering the left end [0..60]: resolves cleanly to gray (128, 128, 128).
        let right_slide = display_list_to_slide(
            &build_dl(150.0, 1.0),
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("partial occlusion on left end must not reject overprint on untouched right end");
        assert_eq!(right_slide.elements.len(), 3);
        assert!(matches!(
            right_slide.elements[2].base(),
            SlideElement::TextBox {
                text,
                color_rgb: (128, 128, 128),
                ..
            } if text == "R"
        ));

        // 2. Overprint over the occluded left end [15..55] (under 50%-opaque blue rect):
        //    hits the occluded region and rejects cleanly.
        assert!(
            display_list_to_slide(
                &build_dl(15.0, 0.5),
                0,
                &fonts,
                &ImageCache::new(),
                &ConversionOptions::default(),
            )
            .is_err(),
            "overprint touching the occluded left end must still reject"
        );
    }

    #[test]
    fn l_shaped_overlapping_missing_mask_strips_preserve_uncovered_corner() {
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (i, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            pdf.extend_from_slice(
                format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
            );
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .as_bytes(),
            );
            pdf
        };

        // Alpha mask: opaque rectangle [10..200, 10..110] + two outline-less Helvetica runs
        // clipped to a horizontal bottom strip [0..160, 0..30] and a vertical left strip
        // [0..30, 0..110], intersecting at [0..30, 0..30] to form an L with empty corner
        // [30..160, 30..110].
        let mask_ops = b"0 g 10 10 190 100 re f \
                         q 0 0 160 30 re W n BT /FH 48 Tf 5 5 Td (AAAAAA) Tj ET Q \
                         q 0 0 30 110 re W n BT /FH 100 Tf 5 5 Td (A) Tj ET Q";

        let run_masked = |page_ops: &[u8]| {
            let pdf = assemble(&[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 120] /Contents 4 0 R \
                  /Resources << /ExtGState << /GSsm 5 0 R >> >> >>"
                    .to_vec(), // 3
                stream("", page_ops),                          // 4
                b"<< /Type /ExtGState /SMask << /Type /Mask /S /Alpha /G 6 0 R >> >>".to_vec(), // 5
                stream(
                    "/Type /XObject /Subtype /Form /BBox [0 0 240 120] \
                     /Group << /Type /Group /S /Transparency /CS /DeviceRGB /I true >> \
                     /Resources << /Font << /FH 7 0 R >> >>",
                    mask_ops,
                ), // 6
                b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(), // 7
            ]);
            let provider = std::sync::Arc::new(zpdf_font::FontProvider::memory_only());
            let doc = zpdf_document::PdfDocument::open(pdf)
                .expect("open pdf")
                .with_font_provider(provider);
            let page = doc.page(0).expect("page 0");
            let mut fonts = doc.load_page_fonts(&page);
            let (content_bytes, _) = doc.page_content_with_diagnostics(&page).expect("content");
            let mut images = ImageCache::new();
            let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
                .with_document(doc.file(), &page.resources)
                .with_fonts(&mut fonts)
                .with_images(&mut images)
                .interpret_with_stats(&content_bytes);
            display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
        };

        // 1. Source fill inside the uncovered corner of the L [60..140, 50..100]: succeeds!
        let corner_slide = run_masked(b"q /GSsm gs 1 0 0 rg 60 50 80 50 re f Q")
            .expect("source fill in uncovered corner of L-shaped missing mask strips must succeed");
        assert_eq!(corner_slide.elements.len(), 1);

        // 2. Source fill touching the horizontal bottom arm [60..140, 10..25] -> errors!
        assert!(
            run_masked(b"q /GSsm gs 1 0 0 rg 60 10 80 15 re f Q").is_err(),
            "source fill touching horizontal arm of L must error"
        );

        // 3. Source fill touching the vertical left arm [10..25, 50..90] -> errors!
        assert!(
            run_masked(b"q /GSsm gs 1 0 0 rg 10 50 15 40 re f Q").is_err(),
            "source fill touching vertical arm of L must error"
        );
    }

    #[test]
    fn nested_alpha_mask_constrains_missing_glyph_inside_outer_mask() {
        let page_rect = Rect::new(0.0, 0.0, 240.0, 120.0);
        let mut fonts = FontCache::new();
        let fid = fonts.insert(
            "FH".into(),
            zpdf_font::LoadedFont::new_placeholder("Helvetica".into()),
        );

        // Inner alpha mask paints ONLY on the left [10..70, 10..110].
        let mut inner_mask_dl = DisplayList::new(page_rect);
        let mut left_rect = zpdf_display_list::Path::new();
        left_rect.rect(Rect::new(10.0, 10.0, 70.0, 110.0));
        inner_mask_dl.push(RenderCommand::FillPath {
            path: left_rect,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let inner_mask = zpdf_display_list::SoftMask {
            kind: zpdf_display_list::SoftMaskKind::Alpha,
            commands: std::sync::Arc::new(inner_mask_dl),
            offset: (0.0, 0.0),
            backdrop_luma: 0.0,
            transfer: None,
        };

        let run_case = |glyph_x: f64, source_rect: Rect| {
            // Outer alpha mask has an opaque rect on the right [160..230, 10..110] and a
            // nested group masked by `inner_mask` (left only) containing an outline-less glyph.
            let mut outer_mask_dl = DisplayList::new(page_rect);
            let mut right_rect = zpdf_display_list::Path::new();
            right_rect.rect(Rect::new(160.0, 10.0, 230.0, 110.0));
            outer_mask_dl.push(RenderCommand::FillPath {
                path: right_rect,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::white()),
                alpha: 1.0,
                overprint: None,
            });
            outer_mask_dl.push(RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: page_rect,
                alpha: 1.0,
                mask: Some(inner_mask.clone()),
            });
            outer_mask_dl.push(RenderCommand::DrawGlyphRun(zpdf_display_list::GlyphRun {
                font_id: fid,
                font_size: 36.0,
                glyphs: vec![zpdf_display_list::PositionedGlyph {
                    glyph_id: 0,
                    code_len: 1,
                    code: b'A' as u32,
                    x: 0.0,
                    y: 0.0,
                    advance: 30.0,
                }],
                paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::white()),
                alpha: 1.0,
                overprint: None,
                transform: zpdf_core::Matrix::translate(glyph_x, 35.0),
                h_scale: 1.0,
            }));
            outer_mask_dl.push(RenderCommand::PopBlendGroup);

            let outer_mask = zpdf_display_list::SoftMask {
                kind: zpdf_display_list::SoftMaskKind::Alpha,
                commands: std::sync::Arc::new(outer_mask_dl),
                offset: (0.0, 0.0),
                backdrop_luma: 0.0,
                transfer: None,
            };
            let mut dl = DisplayList::new(page_rect);
            dl.push(RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: page_rect,
                alpha: 1.0,
                mask: Some(outer_mask),
            });
            let mut src = zpdf_display_list::Path::new();
            src.rect(source_rect);
            dl.push(RenderCommand::FillPath {
                path: src,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::rgb(
                    1.0, 0.0, 0.0,
                )),
                alpha: 1.0,
                overprint: None,
            });
            dl.push(RenderCommand::PopBlendGroup);
            display_list_to_slide(
                &dl,
                0,
                &fonts,
                &ImageCache::new(),
                &ConversionOptions::default(),
            )
        };

        // 1. Glyph on the right (x=180) is masked away completely by the left-only inner
        //    alpha mask: right-hand source fill [170..225] must succeed without error!
        let clipped_away = run_case(180.0, Rect::new(170.0, 20.0, 225.0, 100.0)).expect(
            "glyph masked away by nested left-only alpha mask must not abort right-hand fill",
        );
        assert_eq!(clipped_away.elements.len(), 1);

        // 2. Glyph on the left (x=20) sits INSIDE the left-only inner alpha mask:
        //    left-hand source fill [15..65] must report missing outlines!
        assert!(
            run_case(20.0, Rect::new(15.0, 20.0, 65.0, 100.0)).is_err(),
            "glyph visible through nested left-only alpha mask must error for overlapping left fill"
        );
    }

    #[test]
    fn vertically_aligned_disjoint_translucent_labels_sweep_in_n_log_n() {
        let page_rect = Rect::new(0.0, 0.0, 200.0, 10_000.0);
        let mut fonts = FontCache::new();
        let fid = fonts.insert(
            "FH".into(),
            zpdf_font::LoadedFont::new_placeholder("Helvetica".into()),
        );
        let op_k_half = Some(zpdf_display_list::Overprint {
            cmyk: [0.0, 0.0, 0.0, 0.5],
            active: zpdf_display_list::Overprint::K,
        });

        // 600 vertically separated translucent neutral-gray labels aligned at the EXACT
        // same x coordinate [20..50], spanned by one tall opaque K-only 50% overprint.
        let mut dl = DisplayList::new(page_rect);
        for i in 0..600 {
            let y = 20.0 + i as f64 * 16.0;
            dl.push(RenderCommand::DrawGlyphRun(zpdf_display_list::GlyphRun {
                font_id: fid,
                font_size: 10.0,
                glyphs: vec![zpdf_display_list::PositionedGlyph {
                    glyph_id: 0,
                    code_len: 1,
                    code: b'V' as u32,
                    x: 0.0,
                    y: 0.0,
                    advance: 30.0,
                }],
                paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::gray(0.5)),
                alpha: 0.5,
                overprint: None,
                transform: zpdf_core::Matrix::translate(20.0, y),
                h_scale: 1.0,
            }));
        }
        // Tall overprinting run spanning the entire column (y in [0..9700], x in [20..50]).
        dl.push(RenderCommand::DrawGlyphRun(zpdf_display_list::GlyphRun {
            font_id: fid,
            font_size: 9_650.0,
            glyphs: vec![zpdf_display_list::PositionedGlyph {
                glyph_id: 0,
                code_len: 1,
                code: b'T' as u32,
                x: 0.0,
                y: 0.0,
                advance: 30.0,
            }],
            paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::gray(0.5)),
            alpha: 1.0,
            overprint: op_k_half,
            transform: zpdf_core::Matrix::translate(20.0, 10.0),
            h_scale: 1.0,
        }));

        let (slide, stats) = display_list_to_slide_internal(
            &dl,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
            Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
            None,
        )
        .expect("600 vertically separated translucent gray labels under spanning K overprint");
        assert_eq!(slide.elements.len(), 601);
        assert!(matches!(
            slide.elements[600].base(),
            SlideElement::TextBox {
                text,
                color_rgb: (128, 128, 128),
                ..
            } if text == "T"
        ));
        // Quadratic pairwise checks across 600 column-aligned labels would do 179,700
        // comparisons; the 2D segment-tree sweep completes in O(N log N) (< 40,000).
        assert!(
            stats.native_text_record_visits < 40_000,
            "column-aligned translucent labels must sweep in O(N log N) visits (< 40000), got {}",
            stats.native_text_record_visits
        );

        // Adding one vertically overlapping label in the same column must still reject.
        dl.commands.insert(
            600,
            RenderCommand::DrawGlyphRun(zpdf_display_list::GlyphRun {
                font_id: fid,
                font_size: 10.0,
                glyphs: vec![zpdf_display_list::PositionedGlyph {
                    glyph_id: 0,
                    code_len: 1,
                    code: b'V' as u32,
                    x: 0.0,
                    y: 0.0,
                    advance: 30.0,
                }],
                paint: zpdf_display_list::Paint::Solid(zpdf_display_list::Color::gray(0.5)),
                alpha: 0.5,
                overprint: None,
                transform: zpdf_core::Matrix::translate(20.0, 22.0),
                h_scale: 1.0,
            }),
        );
        assert!(
            display_list_to_slide(
                &dl,
                0,
                &fonts,
                &ImageCache::new(),
                &ConversionOptions::default(),
            )
            .is_err(),
            "vertically overlapping translucent labels in the same column must still reject"
        );
    }

    #[test]
    fn tiling_pattern_fill_and_stroke_export_as_clipped_slide_images() {
        use std::sync::Arc;
        use zpdf_core::Point;
        use zpdf_display_list::{FillRule, Path, StrokeStyle, TilingPattern};

        // Two-color 20×20 tile: red left half [0..10], blue right half [10..20].
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        let mut left = Path::new();
        left.rect(Rect::new(0.0, 0.0, 10.0, 20.0));
        cell.push(RenderCommand::FillPath {
            path: left,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        let mut right = Path::new();
        right.rect(Rect::new(10.0, 0.0, 20.0, 20.0));
        cell.push(RenderCommand::FillPath {
            path: right,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = Arc::new(TilingPattern {
            commands: Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 20.0, 20.0),
            x_step: 20.0,
            y_step: 20.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        });

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 200.0, 200.0));
        // 1) Pattern-filled rectangle [20..100] × [100..160].
        let mut rect_path = Path::new();
        rect_path.rect(Rect::new(20.0, 100.0, 100.0, 160.0));
        dl.push(RenderCommand::FillPath {
            path: rect_path,
            rule: FillRule::NonZero,
            paint: Paint::Tiling(Arc::clone(&tiling)),
            alpha: 1.0,
            overprint: None,
        });
        // 2) Pattern-stroked diagonal line from (20, 20) to (100, 80) with width 16.
        let mut line_path = Path::new();
        line_path.move_to(Point::new(20.0, 20.0));
        line_path.line_to(Point::new(100.0, 80.0));
        dl.push(RenderCommand::StrokePath {
            path: line_path,
            style: StrokeStyle {
                width: 16.0,
                ..StrokeStyle::default()
            },
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
        });

        let fonts = FontCache::new();
        let images = ImageCache::new();
        let slide = display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
            .expect("display_list_to_slide");

        assert_eq!(
            slide.elements.len(),
            2,
            "patterned fill and stroke must emit two slide elements"
        );

        let mut decoded_images = Vec::new();
        for elem in &slide.elements {
            match elem.base() {
                SlideElement::Image {
                    width_emu,
                    height_emu,
                    image_data,
                    ..
                } => {
                    assert!(*width_emu > 0 && *height_emu > 0);
                    let img = image::load_from_memory(image_data)
                        .expect("valid PNG")
                        .to_rgba8();
                    decoded_images.push(img);
                }
                other => panic!(
                    "expected SlideElement::Image for tiling pattern fill/stroke, got {other:?}"
                ),
            }
        }

        // Fill image must contain both opaque red and opaque blue pixels from the tile.
        let fill_img = &decoded_images[0];
        let has_red = fill_img
            .pixels()
            .any(|p| p[0] > 200 && p[1] < 40 && p[2] < 40 && p[3] > 200);
        let has_blue = fill_img
            .pixels()
            .any(|p| p[0] < 40 && p[1] < 40 && p[2] > 200 && p[3] > 200);
        assert!(
            has_red && has_blue,
            "patterned fill image must preserve both tile colors"
        );

        // Stroke image must contain both red and blue along the diagonal stroke
        // AND transparent pixels in the off-stroke corners (clipped to stroke outline).
        let stroke_img = &decoded_images[1];
        let stroke_has_red = stroke_img
            .pixels()
            .any(|p| p[0] > 200 && p[1] < 40 && p[2] < 40 && p[3] > 200);
        let stroke_has_blue = stroke_img
            .pixels()
            .any(|p| p[0] < 40 && p[1] < 40 && p[2] > 200 && p[3] > 200);
        let stroke_has_transparent = stroke_img.pixels().any(|p| p[3] == 0);
        assert!(
            stroke_has_red && stroke_has_blue,
            "patterned stroke image must preserve both tile colors instead of defaulting to black"
        );
        assert!(
            stroke_has_transparent,
            "patterned stroke image must stay transparent outside the stroked outline"
        );

        // Ensure the serialized PPTX archive embeds both rendered PNG media entries.
        let bytes = PptxWriter::new(PptxPresentation {
            slides: vec![slide],
            width_emu: pt_to_emu(200.0),
            height_emu: pt_to_emu(200.0),
        })
        .write_to_bytes()
        .expect("pptx bytes");
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
        assert!(!zip_entry(&mut archive, "ppt/media/image1.png").is_empty());
        assert!(!zip_entry(&mut archive, "ppt/media/image2.png").is_empty());
    }

    /// Composite a `PptxSlide`'s rectangles and PNG images in slide order onto
    /// opaque white paper at `scale` px/pt so tests can assert exact visual
    /// output on the finished slide.
    fn composite_slide_over_white(
        slide: &PptxSlide,
        page_w: f64,
        page_h: f64,
        scale: f64,
    ) -> image::RgbaImage {
        let w = (page_w * scale).round() as u32;
        let h = (page_h * scale).round() as u32;
        let mut canvas = image::RgbaImage::from_pixel(w, h, image::Rgba([255, 255, 255, 255]));
        let emu_to_px = |emu: i64| -> f64 { (emu as f64 / 12700.0) * scale };
        for elem in &slide.elements {
            let mut elem_alpha = 1.0f32;
            let mut cur = elem;
            loop {
                match cur {
                    SlideElement::Opacity { element, alpha } => {
                        elem_alpha *= *alpha;
                        cur = element;
                    }
                    SlideElement::Transformed { element, .. } => {
                        cur = element;
                    }
                    _ => break,
                }
            }
            match cur {
                SlideElement::Rectangle {
                    x_emu,
                    y_emu,
                    width_emu,
                    height_emu,
                    fill_rgb: Some((r, g, b)),
                    ..
                } => {
                    let x0 = emu_to_px(*x_emu).floor().max(0.0) as u32;
                    let y0 = emu_to_px(*y_emu).floor().max(0.0) as u32;
                    let x1 = emu_to_px(*x_emu + *width_emu).ceil().min(w as f64) as u32;
                    let y1 = emu_to_px(*y_emu + *height_emu).ceil().min(h as f64) as u32;
                    for y in y0..y1 {
                        for x in x0..x1 {
                            if elem_alpha >= 1.0 {
                                canvas.put_pixel(x, y, image::Rgba([*r, *g, *b, 255]));
                            } else if elem_alpha > 0.0 {
                                let dst = canvas.get_pixel_mut(x, y);
                                let src = [*r, *g, *b];
                                for c in 0..3 {
                                    let out = src[c] as f32 * elem_alpha
                                        + dst[c] as f32 * (1.0 - elem_alpha);
                                    dst[c] = out.round().clamp(0.0, 255.0) as u8;
                                }
                                dst[3] = 255;
                            }
                        }
                    }
                }
                SlideElement::Image {
                    x_emu,
                    y_emu,
                    width_emu,
                    height_emu,
                    image_data,
                    ..
                } => {
                    let img = image::load_from_memory(image_data)
                        .expect("valid PNG")
                        .to_rgba8();
                    let dx0 = emu_to_px(*x_emu);
                    let dy0 = emu_to_px(*y_emu);
                    let dw = emu_to_px(*width_emu);
                    let dh = emu_to_px(*height_emu);
                    if dw <= 0.0 || dh <= 0.0 {
                        continue;
                    }
                    let px0 = dx0.floor().max(0.0) as u32;
                    let py0 = dy0.floor().max(0.0) as u32;
                    let px1 = (dx0 + dw).ceil().min(w as f64) as u32;
                    let py1 = (dy0 + dh).ceil().min(h as f64) as u32;
                    for py in py0..py1 {
                        let v = ((py as f64 + 0.5 - dy0) / dh * img.height() as f64)
                            .floor()
                            .clamp(0.0, (img.height() - 1) as f64)
                            as u32;
                        for px in px0..px1 {
                            let u = ((px as f64 + 0.5 - dx0) / dw * img.width() as f64)
                                .floor()
                                .clamp(0.0, (img.width() - 1) as f64)
                                as u32;
                            let src = img.get_pixel(u, v);
                            let sa = (src[3] as f32 / 255.0) * elem_alpha;
                            if sa == 0.0 {
                                continue;
                            }
                            let dst = canvas.get_pixel_mut(px, py);
                            for c in 0..3 {
                                let out = src[c] as f32 * sa + dst[c] as f32 * (1.0 - sa);
                                dst[c] = out.round().clamp(0.0, 255.0) as u8;
                            }
                            dst[3] = 255;
                        }
                    }
                }
                _ => {}
            }
        }
        canvas
    }

    #[test]
    fn backdrop_dependent_tiling_pattern_composites_preceding_slide_backdrop() {
        use std::sync::Arc;
        use zpdf_display_list::{BlendMode, FillRule, Path, TilingPattern};

        // 1) Opaque white Multiply cell over a solid blue rectangle must leave
        //    the composited slide blue (never overwriting it with opaque white).
        let make_multiply_tile = |color: Color| -> Arc<TilingPattern> {
            let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
            cell.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Multiply,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 20.0, 20.0),
                alpha: 1.0,
                mask: None,
            });
            let mut cell_rect = Path::new();
            cell_rect.rect(Rect::new(0.0, 0.0, 20.0, 20.0));
            cell.push(RenderCommand::FillPath {
                path: cell_rect,
                rule: FillRule::NonZero,
                paint: Paint::Solid(color),
                alpha: 1.0,
                overprint: None,
            });
            cell.push(RenderCommand::PopBlendGroup);
            Arc::new(TilingPattern {
                commands: Arc::new(cell),
                bbox: Rect::new(0.0, 0.0, 20.0, 20.0),
                x_step: 20.0,
                y_step: 20.0,
                matrix: Matrix::identity(),
                tiling_type: 1,
            })
        };

        let mut dl_white = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        let mut bg = Path::new();
        bg.rect(Rect::new(0.0, 0.0, 100.0, 100.0));
        dl_white.push(RenderCommand::FillPath {
            path: bg.clone(),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        let mut fg = Path::new();
        fg.rect(Rect::new(20.0, 20.0, 60.0, 60.0));
        dl_white.push(RenderCommand::FillPath {
            path: fg.clone(),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(make_multiply_tile(Color::white())),
            alpha: 1.0,
            overprint: None,
        });

        let slide_white = display_list_to_slide(
            &dl_white,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");
        let comp_white = composite_slide_over_white(&slide_white, 100.0, 100.0, 2.0);
        let center_white = comp_white.get_pixel(80, 120);
        assert!(
            center_white[0] < 10 && center_white[1] < 10 && center_white[2] > 245,
            "white Multiply pattern over blue backdrop must stay blue on slide, got {center_white:?}"
        );

        // 2) 50% gray Multiply over solid blue emits a PNG darkened to [0, 0, ~128]
        //    rather than gray [128, 128, 128].
        let mut dl_gray = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        dl_gray.push(RenderCommand::FillPath {
            path: bg,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl_gray.push(RenderCommand::FillPath {
            path: fg,
            rule: FillRule::NonZero,
            paint: Paint::Tiling(make_multiply_tile(Color::rgb(0.5, 0.5, 0.5))),
            alpha: 1.0,
            overprint: None,
        });
        let slide_gray = display_list_to_slide(
            &dl_gray,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");
        let img_bytes = slide_gray
            .elements
            .iter()
            .find_map(|e| match e.base() {
                SlideElement::Image { image_data, .. } => Some(image_data.as_slice()),
                _ => None,
            })
            .expect("gray Multiply pattern image element");
        let img = image::load_from_memory(img_bytes)
            .expect("valid PNG")
            .to_rgba8();
        let center = img.get_pixel(img.width() / 2, img.height() / 2);
        assert!(
            center[0] < 10 && center[1] < 10 && (120..=136).contains(&center[2]) && center[3] > 245,
            "50% gray Multiply over blue must export darkened blue [0,0,~128], got {center:?}"
        );
    }

    #[test]
    fn translucent_backdrop_image_with_hatch_gap_is_composited_only_once() {
        use std::sync::Arc;
        use zpdf_display_list::{BlendMode, FillRule, ImageDraw, Path, TilingPattern};
        use zpdf_image::DecodedImage;

        // Preceding slide element: 50%-opaque black image [0, 0, 0, 128] over [0..100]×[0..100].
        let mut images = ImageCache::new();
        let img_id = images.insert(DecodedImage {
            width: 2,
            height: 2,
            data: [0, 0, 0, 128].repeat(4),
            has_alpha: true,
            is_image_mask: false,
            premultiplied: true,
        });

        // 20×20 Multiply hatch: left half [0..10] painted opaque red Multiply,
        // right half [10..20] left as an UNPAINTED GAP.
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 20.0, 20.0),
            alpha: 1.0,
            mask: None,
        });
        let mut stripe = Path::new();
        stripe.rect(Rect::new(0.0, 0.0, 10.0, 20.0));
        cell.push(RenderCommand::FillPath {
            path: stripe,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);
        let tiling = Arc::new(TilingPattern {
            commands: Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 20.0, 20.0),
            x_step: 20.0,
            y_step: 20.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        });

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        dl.push(RenderCommand::DrawImage(ImageDraw {
            image_id: img_id,
            transform: Matrix::scale(100.0, 100.0),
            alpha: 1.0,
            is_image_mask: false,
        }));
        let mut box_path = Path::new();
        box_path.rect(Rect::new(20.0, 20.0, 80.0, 80.0));
        dl.push(RenderCommand::FillPath {
            path: box_path,
            rule: FillRule::NonZero,
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
        });

        let slide = display_list_to_slide(
            &dl,
            0,
            &FontCache::new(),
            &images,
            &ConversionOptions::default(),
        )
        .expect("slide export");
        assert_eq!(
            slide.elements.len(),
            2,
            "slide must contain translucent backdrop image + hatch overlay PNG"
        );

        let comp = composite_slide_over_white(&slide, 100.0, 100.0, 2.0);
        // Unpainted gap inside hatch (PDF x=35 -> px=70, PDF y=50 -> px y=100):
        // must show 50%-opaque black over white (~127), NEVER doubled 75% black (~64)!
        let gap = comp.get_pixel(70, 100);
        assert!(
            (120..=135).contains(&gap[0])
                && (120..=135).contains(&gap[1])
                && (120..=135).contains(&gap[2]),
            "unpainted hatch gap over 50%-black image must stay ~50% gray (~127), not doubled 75% (~64), got {gap:?}"
        );
        // Painted red Multiply stripe (PDF x=25 -> px=50, PDF y=50 -> px y=100):
        // red Multiply over 50%-black image on white = [~127, 0, 0].
        let stripe_px = comp.get_pixel(50, 100);
        assert!(
            (120..=135).contains(&stripe_px[0]) && stripe_px[1] < 10 && stripe_px[2] < 10,
            "red Multiply stripe over 50%-black image must composite to [~127,0,0], got {stripe_px:?}"
        );
    }

    #[test]
    fn enclosing_blend_group_mask_and_mode_apply_to_patterned_fills_and_strokes() {
        use std::sync::Arc;
        use zpdf_core::Point;
        use zpdf_display_list::{
            BlendMode, FillRule, Path, SoftMask, SoftMaskKind, StrokeStyle, TilingPattern,
        };

        let make_solid_tile = |color: Color| -> Arc<TilingPattern> {
            let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
            let mut sq = Path::new();
            sq.rect(Rect::new(0.0, 0.0, 20.0, 20.0));
            cell.push(RenderCommand::FillPath {
                path: sq,
                rule: FillRule::NonZero,
                paint: Paint::Solid(color),
                alpha: 1.0,
                overprint: None,
            });
            Arc::new(TilingPattern {
                commands: Arc::new(cell),
                bbox: Rect::new(0.0, 0.0, 20.0, 20.0),
                x_step: 20.0,
                y_step: 20.0,
                matrix: Matrix::identity(),
                tiling_type: 1,
            })
        };

        // 1) Normal red tile (both fill and stroke) under an all-zero alpha soft mask
        //    must be completely invisible (no red PNG emitted).
        let zero_mask = SoftMask {
            commands: Arc::new(DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0))),
            kind: SoftMaskKind::Alpha,
            offset: (0.0, 0.0),
            backdrop_luma: 0.0,
            transfer: None,
        };
        let red_tile = make_solid_tile(Color::rgb(1.0, 0.0, 0.0));
        let mut dl_masked = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        dl_masked.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
            alpha: 1.0,
            mask: Some(zero_mask),
        });
        let mut rect_p = Path::new();
        rect_p.rect(Rect::new(10.0, 10.0, 50.0, 50.0));
        dl_masked.push(RenderCommand::FillPath {
            path: rect_p,
            rule: FillRule::NonZero,
            paint: Paint::Tiling(Arc::clone(&red_tile)),
            alpha: 1.0,
            overprint: None,
        });
        let mut line_p = Path::new();
        line_p.move_to(Point::new(10.0, 70.0));
        line_p.line_to(Point::new(90.0, 70.0));
        dl_masked.push(RenderCommand::StrokePath {
            path: line_p,
            style: StrokeStyle {
                width: 10.0,
                ..StrokeStyle::default()
            },
            paint: Paint::Tiling(red_tile),
            alpha: 1.0,
            overprint: None,
        });
        dl_masked.push(RenderCommand::PopBlendGroup);

        let slide_masked = display_list_to_slide(
            &dl_masked,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");
        assert!(
            slide_masked.elements.is_empty(),
            "Normal red patterned fill and stroke under all-zero alpha soft mask must be invisible, got {} elements",
            slide_masked.elements.len()
        );

        // 2) Normal 50%-gray tile inside an enclosing `Multiply` blend group over
        //    a solid blue backdrop must preserve the enclosing Multiply blend mode
        //    for both patterned fill and patterned stroke (darkened blue [0,0,~128]).
        let gray_tile = make_solid_tile(Color::rgb(0.5, 0.5, 0.5));
        let mut dl_blend = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        let mut bg = Path::new();
        bg.rect(Rect::new(0.0, 0.0, 100.0, 100.0));
        dl_blend.push(RenderCommand::FillPath {
            path: bg,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl_blend.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
            alpha: 1.0,
            mask: None,
        });
        let mut fill_r = Path::new();
        fill_r.rect(Rect::new(10.0, 10.0, 40.0, 40.0));
        dl_blend.push(RenderCommand::FillPath {
            path: fill_r,
            rule: FillRule::NonZero,
            paint: Paint::Tiling(Arc::clone(&gray_tile)),
            alpha: 1.0,
            overprint: None,
        });
        let mut stroke_l = Path::new();
        stroke_l.move_to(Point::new(10.0, 75.0));
        stroke_l.line_to(Point::new(90.0, 75.0));
        dl_blend.push(RenderCommand::StrokePath {
            path: stroke_l,
            style: StrokeStyle {
                width: 14.0,
                ..StrokeStyle::default()
            },
            paint: Paint::Tiling(gray_tile),
            alpha: 1.0,
            overprint: None,
        });
        dl_blend.push(RenderCommand::PopBlendGroup);

        let slide_blend = display_list_to_slide(
            &dl_blend,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");
        let comp = composite_slide_over_white(&slide_blend, 100.0, 100.0, 2.0);
        // Inside fill (PDF x=25, y=25 -> px (50, 150)) and stroke (PDF x=50, y=75 -> px (100, 50)):
        for (label, px) in [
            ("enclosing Multiply fill", comp.get_pixel(50, 150)),
            ("enclosing Multiply stroke", comp.get_pixel(100, 50)),
        ] {
            assert!(
                px[0] < 10 && px[1] < 10 && (120..=136).contains(&px[2]),
                "{label} over blue must darken blue to [0,0,~128], not opaque gray, got {px:?}"
            );
        }
    }

    #[test]
    fn wide_finite_miter_limit_is_not_cropped_in_pattern_stroke_bounds() {
        use std::sync::Arc;
        use zpdf_core::Point;
        use zpdf_display_list::{FillRule, LineJoin, Path, StrokeStyle, TilingPattern};

        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        let mut sq = Path::new();
        sq.rect(Rect::new(0.0, 0.0, 10.0, 10.0));
        cell.push(RenderCommand::FillPath {
            path: sq,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = Arc::new(TilingPattern {
            commands: Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        });

        // 1) `stroke_style_grow` must honor actual finite `miter_limit = 200.0` without
        //    clamping to 100.0 (grow = width/2 * 200 = 200pt, not 100pt).
        let style_200 = StrokeStyle {
            width: 2.0,
            join: LineJoin::Miter,
            miter_limit: 200.0,
            ..StrokeStyle::default()
        };
        let grow_200 = stroke_style_grow(&style_200).expect("finite stroke grow");
        assert!(
            (grow_200 - 200.0).abs() < 1e-6,
            "stroke_style_grow must use actual finite miter_limit=200 (grow=200), got {grow_200:?}"
        );

        // Non-finite miter limit returns None (caller falls back to page_rect).
        let inf_style = StrokeStyle {
            width: 2.0,
            join: LineJoin::Miter,
            miter_limit: f32::INFINITY,
            ..StrokeStyle::default()
        };
        assert!(stroke_style_grow(&inf_style).is_none());

        // 2) Acute V corner rendered through tiny-skia (sin(theta) ≈ 0.012 > tiny-skia's
        //    Nearly180 threshold, width = 4, miter_limit = 200): miter tip extends
        //    ~166pt to the left of vertex (200, 100) to x ≈ 34pt, and must be fully
        //    present in the exported slide PNG (not cropped at vertex or page edge).
        let mut wide_v = Path::new();
        wide_v.move_to(Point::new(280.0, 101.2));
        wide_v.line_to(Point::new(200.0, 100.0));
        wide_v.line_to(Point::new(280.0, 98.8));
        let style = StrokeStyle {
            width: 4.0,
            join: LineJoin::Miter,
            miter_limit: 200.0,
            ..StrokeStyle::default()
        };

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 300.0, 200.0));
        dl.push(RenderCommand::StrokePath {
            path: wide_v,
            style,
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
        });
        let slide = display_list_to_slide(
            &dl,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");
        let comp = composite_slide_over_white(&slide, 300.0, 200.0, 2.0);
        // At 2 px/pt, vertex is at px 400 and miter tip reaches ~x = 67pt (px ~134).
        let mut min_red_x = u32::MAX;
        for x in 0..comp.width() {
            let p = comp.get_pixel(x, 200);
            if p[0] > 200 && p[1] < 150 && p[2] < 150 {
                min_red_x = min_red_x.min(x);
            }
        }
        assert!(
            min_red_x < 180,
            "miter tip extending ~133pt past vertex must reach px < 180 (x < 90pt), earliest red px was {min_red_x}"
        );
    }

    #[test]
    fn running_backdrop_scales_linearly_and_rejects_over_budget_partial_rasters() {
        use std::sync::Arc;
        use zpdf_display_list::{BlendMode, FillRule, Path, TilingPattern};

        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
            alpha: 1.0,
            mask: None,
        });
        let mut sq = Path::new();
        sq.rect(Rect::new(0.0, 0.0, 10.0, 10.0));
        cell.push(RenderCommand::FillPath {
            path: sq,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.5, 0.5, 0.5)),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);
        let tiling = Arc::new(TilingPattern {
            commands: Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        });

        // 1) 100 disjoint backdrop-dependent patterned rectangles over a blue page
        //    all export accurately via the O(N) running backdrop.
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 200.0, 200.0));
        let mut bg = Path::new();
        bg.rect(Rect::new(0.0, 0.0, 200.0, 200.0));
        dl.push(RenderCommand::FillPath {
            path: bg,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        for row in 0..10 {
            for col in 0..10 {
                let x0 = col as f64 * 20.0 + 2.0;
                let y0 = row as f64 * 20.0 + 2.0;
                let mut r = Path::new();
                r.rect(Rect::new(x0, y0, x0 + 14.0, y0 + 14.0));
                dl.push(RenderCommand::FillPath {
                    path: r,
                    rule: FillRule::NonZero,
                    paint: Paint::Tiling(Arc::clone(&tiling)),
                    alpha: 1.0,
                    overprint: None,
                });
            }
        }
        let slide = display_list_to_slide(
            &dl,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");
        let img_count = slide
            .elements
            .iter()
            .filter(|e| matches!(e.base(), SlideElement::Image { .. }))
            .count();
        assert_eq!(
            img_count, 100,
            "all 100 disjoint backdrop patterns must export"
        );

        // 2) Zero render budget forces CpuRenderer over-budget immediately:
        //    exporter must surface a time-budget error to the caller instead
        //    of silently returning an incomplete slide.
        let over_budget_err = display_list_to_slide_internal(
            &dl,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
            Some(std::time::Duration::ZERO),
            None,
        )
        .err();
        assert!(
            matches!(
                over_budget_err,
                Some(zpdf_core::Error::StreamDecode(ref msg)) if msg.contains("time budget")
            ),
            "over-budget timeout must surface time-budget error to caller, got {over_budget_err:?}"
        );
    }

    #[test]
    fn overlapping_pattern_siblings_in_isolated_opacity_group_apply_opacity_once() {
        use std::sync::Arc;
        use zpdf_display_list::{BlendMode, FillRule, Path, TilingPattern};

        // Opaque black 10×10 tile.
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        let mut sq = Path::new();
        sq.rect(Rect::new(0.0, 0.0, 10.0, 10.0));
        cell.push(RenderCommand::FillPath {
            path: sq,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        let black_tile = Arc::new(TilingPattern {
            commands: Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        });

        // Isolated Normal group at 50% opacity containing two overlapping
        // opaque black patterned fills: [10..70]×[20..80] and [30..90]×[20..80].
        // Overlap [30..70] must composite to 50% black (~128), NOT 75% black (~64).
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
            alpha: 0.5,
            mask: None,
        });
        let mut left = Path::new();
        left.rect(Rect::new(10.0, 20.0, 70.0, 80.0));
        dl.push(RenderCommand::FillPath {
            path: left,
            rule: FillRule::NonZero,
            paint: Paint::Tiling(Arc::clone(&black_tile)),
            alpha: 1.0,
            overprint: None,
        });
        let mut right = Path::new();
        right.rect(Rect::new(30.0, 20.0, 90.0, 80.0));
        dl.push(RenderCommand::FillPath {
            path: right,
            rule: FillRule::NonZero,
            paint: Paint::Tiling(black_tile),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);

        let slide = display_list_to_slide(
            &dl,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");
        let comp = composite_slide_over_white(&slide, 100.0, 100.0, 2.0);
        // Non-overlapping left (x=20 -> px=40), overlap center (x=50 -> px=100),
        // non-overlapping right (x=80 -> px=160), all at y=50 -> px=100.
        for (label, px) in [
            ("left-only", comp.get_pixel(40, 100)),
            ("overlap", comp.get_pixel(100, 100)),
            ("right-only", comp.get_pixel(160, 100)),
        ] {
            assert!(
                (120..=136).contains(&px[0])
                    && (120..=136).contains(&px[1])
                    && (120..=136).contains(&px[2]),
                "{label} inside isolated 50% group must be 50% black (~128), not 75% (~64), got {px:?}"
            );
        }
    }

    #[test]
    fn long_blend_group_preserves_painted_backdrop_past_32_commands() {
        use std::sync::Arc;
        use zpdf_display_list::{BlendMode, FillRule, Path, TilingPattern};

        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
            alpha: 1.0,
            mask: None,
        });
        let mut sq = Path::new();
        sq.rect(Rect::new(0.0, 0.0, 10.0, 10.0));
        cell.push(RenderCommand::FillPath {
            path: sq,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.5, 0.5, 0.5)),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);
        let gray_mul = Arc::new(TilingPattern {
            commands: Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        });

        for isolated in [true, false] {
            let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
            dl.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Normal,
                isolated,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
                alpha: if isolated { 1.0 } else { 0.99 },
                mask: None,
            });
            // Opaque blue background inside the group.
            let mut bg = Path::new();
            bg.rect(Rect::new(0.0, 0.0, 100.0, 100.0));
            dl.push(RenderCommand::FillPath {
                path: bg,
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
                alpha: 1.0,
                overprint: None,
            });
            // 50 detail marks elsewhere (well past the old 32-command cutoff).
            for i in 0..50 {
                let mut dot = Path::new();
                let x = (i % 10) as f64 * 2.0;
                let y = (i / 10) as f64 * 2.0;
                dot.rect(Rect::new(x, y, x + 1.0, y + 1.0));
                dl.push(RenderCommand::FillPath {
                    path: dot,
                    rule: FillRule::NonZero,
                    paint: Paint::Solid(Color::rgb(0.0, 1.0, 0.0)),
                    alpha: 1.0,
                    overprint: None,
                });
            }
            // Gray Multiply tile over the blue region [40..80]×[40..80].
            let mut fg = Path::new();
            fg.rect(Rect::new(40.0, 40.0, 80.0, 80.0));
            dl.push(RenderCommand::FillPath {
                path: fg,
                rule: FillRule::NonZero,
                paint: Paint::Tiling(Arc::clone(&gray_mul)),
                alpha: 1.0,
                overprint: None,
            });
            dl.push(RenderCommand::PopBlendGroup);

            let slide = display_list_to_slide(
                &dl,
                0,
                &FontCache::new(),
                &ImageCache::new(),
                &ConversionOptions::default(),
            )
            .expect("slide export");
            let comp = composite_slide_over_white(&slide, 100.0, 100.0, 2.0);
            let center = comp.get_pixel(120, 80); // PDF (60, 60) -> px (120, 80)
            assert!(
                center[0] < 12 && center[1] < 12 && (120..=138).contains(&center[2]),
                "isolated={isolated}: gray Multiply after 50+ commands over blue inside group must be dark blue [0,0,~128], not gray, got {center:?}"
            );
        }
    }

    #[test]
    fn page_level_screen_pattern_uses_white_paper_while_isolated_group_stays_transparent() {
        use std::sync::Arc;
        use zpdf_display_list::{BlendMode, FillRule, Path, TilingPattern};

        // Opaque black Screen cell: over white slide paper Screen(1, 0) = white;
        // inside an isolated child group (transparent backdrop), opaque black
        // Screen renders opaque black.
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Screen,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 20.0, 20.0),
            alpha: 1.0,
            mask: None,
        });
        let mut sq = Path::new();
        sq.rect(Rect::new(0.0, 0.0, 20.0, 20.0));
        cell.push(RenderCommand::FillPath {
            path: sq,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);
        let black_screen = Arc::new(TilingPattern {
            commands: Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 20.0, 20.0),
            x_step: 20.0,
            y_step: 20.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        });

        // 1) Page-level opaque black Screen on an empty white slide must stay white.
        let mut dl_page = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        let mut r = Path::new();
        r.rect(Rect::new(20.0, 20.0, 80.0, 80.0));
        dl_page.push(RenderCommand::FillPath {
            path: r.clone(),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(Arc::clone(&black_screen)),
            alpha: 1.0,
            overprint: None,
        });
        let slide_page = display_list_to_slide(
            &dl_page,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");
        let comp_page = composite_slide_over_white(&slide_page, 100.0, 100.0, 2.0);
        let px_page = comp_page.get_pixel(100, 100);
        assert!(
            px_page[0] > 245 && px_page[1] > 245 && px_page[2] > 245,
            "page-level opaque black Screen on empty white slide must stay white, got {px_page:?}"
        );

        // 2) Same opaque black Screen inside an isolated Normal child group
        //    must see transparent group backdrop and paint opaque black.
        let mut dl_iso = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        dl_iso.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
            alpha: 1.0,
            mask: None,
        });
        dl_iso.push(RenderCommand::FillPath {
            path: r,
            rule: FillRule::NonZero,
            paint: Paint::Tiling(black_screen),
            alpha: 1.0,
            overprint: None,
        });
        dl_iso.push(RenderCommand::PopBlendGroup);
        let slide_iso = display_list_to_slide(
            &dl_iso,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");
        let comp_iso = composite_slide_over_white(&slide_iso, 100.0, 100.0, 2.0);
        let px_iso = comp_iso.get_pixel(100, 100);
        assert!(
            px_iso[0] < 10 && px_iso[1] < 10 && px_iso[2] < 10,
            "opaque black Screen inside isolated child group must render opaque black, got {px_iso:?}"
        );
    }

    #[test]
    fn narrow_tiling_pattern_matches_placed_extent_to_raster_pixel_grid() {
        use std::sync::Arc;
        use zpdf_display_list::{FillRule, Path, TilingPattern};

        // At 2 px/pt, a 0.25-pt-wide opaque red rectangle rounds up to 1 pixel
        // wide (0.5 pt). The slide image width must match the 1-pixel raster
        // extent (0.5 pt) instead of shrinking it back down to 0.25 pt.
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        let mut r = Path::new();
        r.rect(Rect::new(0.0, 0.0, 10.0, 10.0));
        cell.push(RenderCommand::FillPath {
            path: r,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = Arc::new(TilingPattern {
            commands: Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        });

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        let mut thin = Path::new();
        thin.rect(Rect::new(10.0, 10.0, 10.25, 20.0));
        dl.push(RenderCommand::FillPath {
            path: thin,
            rule: FillRule::NonZero,
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
        });

        let slide = display_list_to_slide(
            &dl,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");
        let (x_emu, width_emu, height_emu, image_data) = slide
            .elements
            .iter()
            .find_map(|e| match e.base() {
                SlideElement::Image {
                    x_emu,
                    width_emu,
                    height_emu,
                    image_data,
                    ..
                } => Some((*x_emu, *width_emu, *height_emu, image_data.as_slice())),
                _ => None,
            })
            .expect("narrow pattern image");
        let img = image::load_from_memory(image_data)
            .expect("valid PNG")
            .to_rgba8();
        let x_pt = x_emu as f64 / 12700.0;
        let placed_w_pt = width_emu as f64 / 12700.0;
        let placed_h_pt = height_emu as f64 / 12700.0;
        assert!(
            x_pt <= 10.0 + 1e-3 && x_pt + placed_w_pt >= 10.25 - 1e-3,
            "placed box must cover [10.0..10.25], got [{x_pt}..{}]",
            x_pt + placed_w_pt
        );
        assert!(placed_h_pt >= 10.0 - 1e-3);
        // Total horizontal ink across the middle row = sum(alpha_x) * (placed_w / img.width)
        // must preserve the full 0.25pt stripe width rather than shrinking it via unaligned
        // pixel rounding (~0.125pt).
        let mid_y = img.height() / 2;
        let px_w_pt = placed_w_pt / img.width() as f64;
        let mut ink_w_pt = 0.0;
        for x in 0..img.width() {
            let px = img.get_pixel(x, mid_y);
            if px[3] > 0 {
                assert!(
                    px[0] > 240 && px[1] < 10 && px[2] < 10,
                    "ink pixel at x={x} should be pure red, got {px:?}"
                );
                ink_w_pt += (px[3] as f64 / 255.0) * px_w_pt;
            }
        }
        assert!(
            (0.22..=0.28).contains(&ink_w_pt),
            "narrow 0.25pt tiling stripe must preserve ~0.25pt total horizontal ink on slide, got {ink_w_pt:.4}pt (img={}x{}, placed_w={placed_w_pt:.4}pt)",
            img.width(),
            img.height()
        );
    }

    #[test]
    fn patterned_type3_glyph_run_exports_as_raster_image_with_clip_and_opacity() {
        use std::sync::Arc;
        use zpdf_core::Matrix;
        use zpdf_display_list::{
            FillRule, GlyphRun, Paint, Path, PositionedGlyph, RenderCommand, TilingPattern,
        };
        use zpdf_font::{CidWidths, FontCache, LoadedFont, PdfFontType};

        // 8×8 uncolored Type 3 bitmap glyph (`/IM true`): top 4 rows opaque ink, bottom 4 transparent.
        let charproc =
            b"1 0 0 0 1 1 d1 BI /W 8 /H 8 /BPC 1 /IM true ID \x00\x00\x00\x00\xff\xff\xff\xff EI";
        let mut encoding = vec![String::new(); 66];
        encoding[65] = "A".into();
        let font = LoadedFont::new_with_data(
            PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: std::collections::HashMap::from([(
                    "A".into(),
                    Arc::<[u8]>::from(&charproc[..]),
                )]),
                encoding,
                widths: vec![1.0],
                first_char: 65,
            },
            "Type3PptxPattern".into(),
            Vec::new(),
            CidWidths::new(1.0),
        );
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        // 8×8 tiling cell: left 4pt red, right 4pt blue.
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 8.0, 8.0));
        let mut left = Path::new();
        left.rect(Rect::new(0.0, 0.0, 4.0, 8.0));
        cell.push(RenderCommand::FillPath {
            path: left,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        let mut right = Path::new();
        right.rect(Rect::new(4.0, 0.0, 8.0, 8.0));
        cell.push(RenderCommand::FillPath {
            path: right,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = Arc::new(TilingPattern {
            commands: Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 8.0, 8.0),
            x_step: 8.0,
            y_step: 8.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        });

        // Clip to x=[8..24] so only the left 16pt of the 24pt glyph renders,
        // and apply 50% glyph opacity.
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 40.0, 40.0));
        let mut clip = Path::new();
        clip.rect(Rect::new(8.0, 8.0, 24.0, 32.0));
        dl.push(RenderCommand::PushClip {
            path: clip,
            rule: FillRule::NonZero,
        });
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 24.0,
            glyphs: vec![PositionedGlyph {
                glyph_id: 65,
                code_len: 1,
                code: b'A' as u32,
                x: 0.0,
                y: 0.0,
                advance: 24.0,
            }],
            paint: Paint::Tiling(tiling),
            alpha: 0.5,
            overprint: None,
            transform: Matrix::translate(8.0, 8.0),
            h_scale: 1.0,
        }));
        dl.push(RenderCommand::PopClip);

        let slide = display_list_to_slide(
            &dl,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");
        assert!(
            !slide
                .elements
                .iter()
                .any(|e| matches!(e.base(), SlideElement::TextBox { .. })),
            "patterned Type 3 glyph run must not emit a black TextBox"
        );
        assert!(
            slide
                .elements
                .iter()
                .any(|e| matches!(e.base(), SlideElement::Image { .. })),
            "patterned Type 3 glyph run must emit a raster SlideElement::Image"
        );

        let comp = composite_slide_over_white(&slide, 40.0, 40.0, 2.0);
        // Top half (PDF y=26 -> px y=28): x=10 (px x=20) is 50% red over white [255, ~128, ~128];
        // x=14 (px x=28) is 50% blue over white [~128, ~128, 255];
        // clipped region x=28 (px x=56) and transparent bottom half (PDF y=14 -> px y=52) stay white.
        let red_px = comp.get_pixel(20, 28);
        let blue_px = comp.get_pixel(28, 28);
        let clipped_px = comp.get_pixel(56, 28);
        let bottom_px = comp.get_pixel(20, 52);
        assert!(
            red_px[0] > 245 && (118..=138).contains(&red_px[1]) && (118..=138).contains(&red_px[2]),
            "patterned Type 3 glyph red stripe at 50% opacity must composite to pink, got {red_px:?}"
        );
        assert!(
            (118..=138).contains(&blue_px[0])
                && (118..=138).contains(&blue_px[1])
                && blue_px[2] > 245,
            "patterned Type 3 glyph blue stripe at 50% opacity must composite to light blue, got {blue_px:?}"
        );
        assert_eq!(clipped_px, &image::Rgba([255, 255, 255, 255]));
        assert_eq!(bottom_px, &image::Rgba([255, 255, 255, 255]));
    }

    #[test]
    fn zero_alpha_image_and_diverged_fill_prefix_stay_consistent_with_screen_pattern() {
        use std::sync::Arc;
        use zpdf_core::Matrix;
        use zpdf_display_list::{
            BlendMode, FillRule, ImageDraw, Paint, Path, RenderCommand, TilingPattern,
        };
        use zpdf_image::DecodedImage;

        let mut images = ImageCache::new();
        let blue_id = images.insert(DecodedImage {
            width: 2,
            height: 2,
            data: vec![
                0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255,
            ],
            has_alpha: false,
            is_image_mask: false,
            premultiplied: false,
        });

        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Screen,
            isolated: false,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
            alpha: 1.0,
            mask: None,
        });
        let mut r = Path::new();
        r.rect(Rect::new(0.0, 0.0, 10.0, 10.0));
        cell.push(RenderCommand::FillPath {
            path: r,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);
        let white_screen = Arc::new(TilingPattern {
            commands: Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        });

        // Case 1: Opaque blue image with `ImageDraw.alpha = 0` under white Screen pattern
        // must leave the slide white (never blue).
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 40.0, 40.0));
        dl.push(RenderCommand::DrawImage(ImageDraw {
            image_id: blue_id,
            transform: Matrix::scale(40.0, 40.0),
            alpha: 0.0,
            is_image_mask: false,
        }));
        let mut box_path = Path::new();
        box_path.rect(Rect::new(10.0, 10.0, 30.0, 30.0));
        dl.push(RenderCommand::FillPath {
            path: box_path.clone(),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(Arc::clone(&white_screen)),
            alpha: 1.0,
            overprint: None,
        });

        let slide = display_list_to_slide(
            &dl,
            0,
            &FontCache::new(),
            &images,
            &ConversionOptions::default(),
        )
        .expect("slide export");
        let comp = composite_slide_over_white(&slide, 40.0, 40.0, 2.0);
        let mid = comp.get_pixel(40, 40);
        assert_eq!(
            mid,
            &image::Rgba([255, 255, 255, 255]),
            "zero-alpha blue image under white Screen pattern must stay white, got {mid:?}"
        );

        // Case 2: Native translucent FillPath (`alpha = 0.0`, which native OOXML
        // shapes ignore) followed by white Screen pattern: `prefix_diverged`
        // must replace the affected region with the true white composite instead
        // of leaving the native blue shape visible.
        let mut dl2 = DisplayList::new(Rect::new(0.0, 0.0, 40.0, 40.0));
        let mut bg = Path::new();
        bg.rect(Rect::new(0.0, 0.0, 40.0, 40.0));
        dl2.push(RenderCommand::FillPath {
            path: bg,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 0.0,
            overprint: None,
        });
        dl2.push(RenderCommand::FillPath {
            path: box_path,
            rule: FillRule::NonZero,
            paint: Paint::Tiling(white_screen),
            alpha: 1.0,
            overprint: None,
        });
        let slide2 = display_list_to_slide(
            &dl2,
            0,
            &FontCache::new(),
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");
        let comp2 = composite_slide_over_white(&slide2, 40.0, 40.0, 2.0);
        let mid2 = comp2.get_pixel(40, 40);
        assert_eq!(
            mid2,
            &image::Rgba([255, 255, 255, 255]),
            "diverged native fill prefix under white Screen pattern must replace affected region with white, got {mid2:?}"
        );
    }

    #[test]
    fn patterned_glyph_after_translucent_prefix_limits_replacement_to_glyph_ink_bounds() {
        use std::sync::Arc;
        use zpdf_core::Matrix;
        use zpdf_display_list::{
            BlendMode, FillRule, GlyphRun, Paint, Path, PositionedGlyph, RenderCommand,
            TilingPattern,
        };
        use zpdf_font::{CidWidths, FontCache, LoadedFont, PdfFontType};

        let charproc = b"1 0 0 0 1 1 d1 0 0 1 1 re f";
        let mut encoding = vec![String::new(); 66];
        encoding[65] = "A".into();
        let type3_font = LoadedFont::new_with_data(
            PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: std::collections::HashMap::from([(
                    "A".into(),
                    Arc::<[u8]>::from(&charproc[..]),
                )]),
                encoding,
                widths: vec![1.0],
                first_char: 65,
            },
            "Type3TightBounds".into(),
            Vec::new(),
            CidWidths::new(1.0),
        );
        let text_font = LoadedFont::new_standard("Helvetica".into()).expect("Helvetica font");
        let mut fonts = FontCache::new();
        let t3_id = fonts.insert("F1".into(), type3_font);
        let text_id = fonts.insert("F2".into(), text_font);

        // Backdrop-dependent Multiply blue tiling pattern.
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 8.0, 8.0));
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: false,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 8.0, 8.0),
            alpha: 1.0,
            mask: None,
        });
        let mut r = Path::new();
        r.rect(Rect::new(0.0, 0.0, 8.0, 8.0));
        cell.push(RenderCommand::FillPath {
            path: r,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);
        let tiling = Arc::new(TilingPattern {
            commands: Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 8.0, 8.0),
            x_step: 8.0,
            y_step: 8.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        });

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 300.0, 300.0));
        // 1. Translucent native rectangle sets `prefix_diverged = true`.
        let mut prefix_rect = Path::new();
        prefix_rect.rect(Rect::new(10.0, 10.0, 60.0, 60.0));
        dl.push(RenderCommand::FillPath {
            path: prefix_rect,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 0.5,
            overprint: None,
        });
        // 2. Distant native TextBox at (200, 200) that must stay exposed.
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id: text_id,
            font_size: 14.0,
            glyphs: vec![
                PositionedGlyph {
                    glyph_id: b'O' as u16,
                    code_len: 1,
                    code: b'O' as u32,
                    x: 0.0,
                    y: 0.0,
                    advance: 10.0,
                },
                PositionedGlyph {
                    glyph_id: b'K' as u16,
                    code_len: 1,
                    code: b'K' as u32,
                    x: 10.0,
                    y: 0.0,
                    advance: 10.0,
                },
            ],
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
            transform: Matrix::translate(200.0, 200.0),
            h_scale: 1.0,
        }));
        // 3. Small unclipped backdrop-dependent patterned glyph at [20..44]×[20..44].
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id: t3_id,
            font_size: 24.0,
            glyphs: vec![PositionedGlyph {
                glyph_id: 65,
                code_len: 1,
                code: b'A' as u32,
                x: 0.0,
                y: 0.0,
                advance: 24.0,
            }],
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
            transform: Matrix::translate(20.0, 20.0),
            h_scale: 1.0,
        }));

        let slide = display_list_to_slide(
            &dl,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");
        let text_box = slide
            .elements
            .iter()
            .find_map(|e| match e.base() {
                SlideElement::TextBox {
                    text,
                    x_emu,
                    y_emu,
                    width_emu,
                    height_emu,
                    ..
                } => Some((text.as_str(), *x_emu, *y_emu, *width_emu, *height_emu)),
                _ => None,
            })
            .expect("distant native TextBox must remain on the slide");
        assert_eq!(text_box.0, "OK");

        let img_rect = slide
            .elements
            .iter()
            .find_map(|e| match e.base() {
                SlideElement::Image {
                    x_emu,
                    y_emu,
                    width_emu,
                    height_emu,
                    ..
                } => Some((*x_emu, *y_emu, *width_emu, *height_emu)),
                _ => None,
            })
            .expect("patterned glyph replacement image");
        // Image bounds must stay tightly bounded to the 24×24pt glyph (<= 30pt with padding)
        // and must NOT overlap the distant TextBox at x=200pt.
        assert!(
            img_rect.2 < pt_to_emu(35.0) && img_rect.3 < pt_to_emu(35.0),
            "replacement PNG must be bounded to glyph ink bounds (~26pt), got {}×{} pt",
            img_rect.2 as f64 / 12700.0,
            img_rect.3 as f64 / 12700.0
        );
        let img_x1 = img_rect.0 + img_rect.2;
        assert!(
            img_x1 < text_box.1,
            "replacement PNG right edge ({}pt) must stay strictly left of distant TextBox ({}pt)",
            img_x1 as f64 / 12700.0,
            text_box.1 as f64 / 12700.0
        );
    }

    #[test]
    fn sheared_image_before_screen_pattern_replaces_pptx_corner() {
        use std::sync::Arc;
        use zpdf_core::Matrix;
        use zpdf_display_list::{
            BlendMode, FillRule, ImageDraw, Paint, Path, RenderCommand, TilingPattern,
        };
        use zpdf_image::DecodedImage;

        let mut images = ImageCache::new();
        let blue_id = images.insert(DecodedImage {
            width: 2,
            height: 2,
            data: vec![
                0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255,
            ],
            has_alpha: false,
            is_image_mask: false,
            premultiplied: false,
        });

        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Screen,
            isolated: false,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
            alpha: 1.0,
            mask: None,
        });
        let mut r = Path::new();
        r.rect(Rect::new(0.0, 0.0, 10.0, 10.0));
        cell.push(RenderCommand::FillPath {
            path: r,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);
        let white_screen = Arc::new(TilingPattern {
            commands: Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        });

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        // Unclipped sheared opaque blue RGB image:
        // PDF parallelogram corners: (10,20), (50,20), (30,60), (70,60).
        // PowerPoint orthogonal rectangle: x=[20..60], PDF y=[17.64..62.36].
        // PDF point (22, 56) is WHITE in CPU replay, but BLUE in PowerPoint's orthogonal image box.
        dl.push(RenderCommand::DrawImage(ImageDraw {
            image_id: blue_id,
            transform: Matrix::new(40.0, 0.0, 20.0, 40.0, 10.0, 20.0),
            alpha: 1.0,
            is_image_mask: false,
        }));
        let mut corner = Path::new();
        corner.rect(Rect::new(15.0, 45.0, 35.0, 65.0));
        dl.push(RenderCommand::FillPath {
            path: corner,
            rule: FillRule::NonZero,
            paint: Paint::Tiling(white_screen),
            alpha: 1.0,
            overprint: None,
        });

        let slide = display_list_to_slide(
            &dl,
            0,
            &FontCache::new(),
            &images,
            &ConversionOptions::default(),
        )
        .expect("slide export");
        let comp = composite_slide_over_white(&slide, 100.0, 100.0, 2.0);
        // PDF (22, 56) -> PPT (22, 44) -> pixel (44, 88).
        let px = comp.get_pixel(44, 88);
        assert_eq!(
            px,
            &image::Rgba([255, 255, 255, 255]),
            "white Screen pattern over corner covered only by PowerPoint's orthogonal sheared-image box must become white, got {px:?}"
        );
    }

    #[test]
    fn type3_named_resource_stencil_do_bounds_and_renders_on_slide() {
        use std::sync::Arc;
        use zpdf_core::Matrix;
        use zpdf_display_list::{FillRule, Paint, Path, RenderCommand, TilingPattern};
        use zpdf_font::{
            CidWidths, FontCache, LoadedFont, PdfFontType, Type3ImageXObject, Type3Resources,
            Type3XObject,
        };

        let mut img_dict = zpdf_core::PdfDict::new();
        img_dict.insert(
            zpdf_core::PdfName::new("Subtype"),
            zpdf_core::PdfObject::Name(zpdf_core::PdfName::new("Image")),
        );
        img_dict.insert(
            zpdf_core::PdfName::new("Width"),
            zpdf_core::PdfObject::Integer(8),
        );
        img_dict.insert(
            zpdf_core::PdfName::new("Height"),
            zpdf_core::PdfObject::Integer(8),
        );
        img_dict.insert(
            zpdf_core::PdfName::new("BitsPerComponent"),
            zpdf_core::PdfObject::Integer(1),
        );
        img_dict.insert(
            zpdf_core::PdfName::new("ImageMask"),
            zpdf_core::PdfObject::Bool(true),
        );
        let mut resources = Type3Resources::default();
        resources.xobjects.insert(
            "Ink".into(),
            Type3XObject::Image(Arc::new(Type3ImageXObject {
                dict: img_dict,
                data: Arc::from([0u8; 8].as_slice()),
                smask: None,
                mask_stream: None,
            })),
        );

        let named_stream = b"1 0 0 0 1 1 d1 /Ink Do";
        let font = LoadedFont::new_with_data(
            PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: std::collections::HashMap::from([(
                    "A".into(),
                    Arc::<[u8]>::from(&named_stream[..]),
                )]),
                encoding: vec!["A".into()],
                widths: vec![1.0],
                first_char: 0,
            },
            "Type3NamedPptx".into(),
            Vec::new(),
            CidWidths::new(1.0),
        )
        .with_type3_resources(Arc::new(resources));
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 8.0, 8.0));
        let mut r = Path::new();
        r.rect(Rect::new(0.0, 0.0, 8.0, 8.0));
        cell.push(RenderCommand::FillPath {
            path: r,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = Arc::new(TilingPattern {
            commands: Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 8.0, 8.0),
            x_step: 8.0,
            y_step: 8.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        });

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 24.0,
            glyphs: vec![PositionedGlyph {
                glyph_id: 0,
                code_len: 1,
                code: b'A' as u32,
                x: 0.0,
                y: 0.0,
                advance: 24.0,
            }],
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
            transform: Matrix::translate(20.0, 20.0),
            h_scale: 1.0,
        }));

        let slide = display_list_to_slide(
            &dl,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");
        let comp = composite_slide_over_white(&slide, 100.0, 100.0, 1.0);
        // PDF (32, 32) -> PPT (32, 68).
        let glyph_px = comp.get_pixel(32, 68);
        let outside_px = comp.get_pixel(5, 5);
        assert!(
            glyph_px[0] < 15 && glyph_px[1] < 15 && glyph_px[2] > 240,
            "named `/Ink Do` Type 3 glyph must render blue on PPTX slide, got {glyph_px:?}"
        );
        assert_eq!(outside_px, &image::Rgba([255, 255, 255, 255]));
    }

    #[test]
    fn type3_glyph_bounds_cached_across_repeated_occurrences_and_honors_deadline() {
        use std::sync::Arc;
        use std::time::Duration;
        use zpdf_core::Matrix;
        use zpdf_display_list::{FillRule, Paint, Path, RenderCommand, TilingPattern};
        use zpdf_font::{CidWidths, FontCache, LoadedFont, PdfFontType};

        let inline_stream =
            b"1 0 0 0 1 1 d1 BI /W 8 /H 8 /BPC 1 /IM true ID \x00\x00\x00\x00\x00\x00\x00\x00 EI";
        let font = LoadedFont::new_with_data(
            PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: std::collections::HashMap::from([(
                    "A".into(),
                    Arc::<[u8]>::from(&inline_stream[..]),
                )]),
                encoding: vec!["A".into()],
                widths: vec![1.0],
                first_char: 0,
            },
            "Type3CachedBounds".into(),
            Vec::new(),
            CidWidths::new(1.0),
        );
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 8.0, 8.0));
        let mut r = Path::new();
        r.rect(Rect::new(0.0, 0.0, 8.0, 8.0));
        cell.push(RenderCommand::FillPath {
            path: r,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = Arc::new(TilingPattern {
            commands: Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 8.0, 8.0),
            x_step: 8.0,
            y_step: 8.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        });

        let glyphs: Vec<PositionedGlyph> = (0..500)
            .map(|i| PositionedGlyph {
                glyph_id: 0,
                code_len: 1,
                code: b'A' as u32,
                x: (i % 25) as f32 * 4.0,
                y: (i / 25) as f32 * 4.0,
                advance: 4.0,
            })
            .collect();
        let run = GlyphRun {
            font_id,
            font_size: 4.0,
            glyphs,
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
            transform: Matrix::translate(10.0, 10.0),
            h_scale: 1.0,
        };

        let images = ImageCache::new();
        let options = ConversionOptions::default();
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 200.0, 200.0));
        dl.push(RenderCommand::DrawGlyphRun(run));

        let (slide, stats) = display_list_to_slide_internal(
            &dl,
            0,
            &fonts,
            &images,
            &options,
            Some(zpdf_render_cpu::DEFAULT_RENDER_BUDGET),
            None,
        )
        .expect("500 repeated Type 3 glyphs within budget");
        assert_eq!(
            stats.type3_bounds_cache_entries, 1,
            "500 occurrences of one Type 3 glyph must populate only 1 cache entry"
        );
        let (x_pt, y_pt, w_pt, h_pt) = slide
            .elements
            .iter()
            .find_map(|e| match e.base() {
                SlideElement::Image {
                    x_emu,
                    y_emu,
                    width_emu,
                    height_emu,
                    ..
                } => Some((
                    *x_emu as f64 / 12700.0,
                    *y_emu as f64 / 12700.0,
                    *width_emu as f64 / 12700.0,
                    *height_emu as f64 / 12700.0,
                )),
                _ => None,
            })
            .expect("patterned Type 3 run must emit an image");
        // Page height is 200; PDF y=[10..90] maps to PPTX top-down y=[110..190], x=[10..110].
        assert!(
            x_pt <= 10.5 && x_pt + w_pt >= 109.5 && y_pt <= 110.5 && y_pt + h_pt >= 189.5,
            "cached bounds must cover all 500 placements, got x={x_pt}..{} y={y_pt}..{}",
            x_pt + w_pt,
            y_pt + h_pt
        );

        // Zero budget propagates a time-budget error from Type 3 bounds measurement.
        match display_list_to_slide_internal(
            &dl,
            0,
            &fonts,
            &images,
            &options,
            Some(Duration::ZERO),
            None,
        ) {
            Err(zpdf_core::Error::StreamDecode(msg)) => {
                assert!(msg.contains("time budget"), "unexpected error: {msg}");
            }
            Ok(_) => panic!("zero budget must time out during Type 3 bounds measurement"),
            Err(other) => panic!("expected time-budget StreamDecode error, got {other:?}"),
        }
    }

    #[test]
    fn spaced_native_text_before_white_screen_pattern_replaces_gap() {
        use std::sync::Arc;
        use zpdf_core::Matrix;
        use zpdf_display_list::{BlendMode, FillRule, Paint, Path, RenderCommand, TilingPattern};
        use zpdf_font::{CidWidths, FontCache, LoadedFont, PdfFontType};

        const PRIMARY_TTF: &[u8] =
            include_bytes!("../../zpdf-font/tests/fixtures/coverage-primary.ttf");
        let text_font = LoadedFont::new_with_data(
            PdfFontType::TrueType,
            "SpacedTestTTF".into(),
            PRIMARY_TTF.to_vec(),
            CidWidths::new(600.0),
        );
        let gid_a = text_font.unicode_glyph('A' as u32).unwrap().0;
        let gid_b = text_font.unicode_glyph('B' as u32).unwrap().0;
        let mut fonts = FontCache::new();
        let text_id = fonts.insert("F1".into(), text_font);

        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Screen,
            isolated: false,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
            alpha: 1.0,
            mask: None,
        });
        let mut r = Path::new();
        r.rect(Rect::new(0.0, 0.0, 10.0, 10.0));
        cell.push(RenderCommand::FillPath {
            path: r,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);
        let white_screen = Arc::new(TilingPattern {
            commands: Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        });

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 120.0, 80.0));
        // Opaque "AB" run with large Tc spacing: 'A' at x=10, 'B' pushed out to x=70.
        // PowerPoint emits a single "AB" TextBox with normal kerning, placing 'B' near x=20..30.
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id: text_id,
            font_size: 18.0,
            glyphs: vec![
                PositionedGlyph {
                    glyph_id: gid_a,
                    code_len: 1,
                    code: b'A' as u32,
                    x: 0.0,
                    y: 0.0,
                    advance: 60.0,
                },
                PositionedGlyph {
                    glyph_id: gid_b,
                    code_len: 1,
                    code: b'B' as u32,
                    x: 60.0,
                    y: 0.0,
                    advance: 12.0,
                },
            ],
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
            transform: Matrix::translate(10.0, 30.0),
            h_scale: 1.0,
        }));
        // White Screen pattern over the PDF gap [22..55]×[25..55] (where PowerPoint places 'B').
        let mut gap = Path::new();
        gap.rect(Rect::new(22.0, 25.0, 55.0, 55.0));
        dl.push(RenderCommand::FillPath {
            path: gap,
            rule: FillRule::NonZero,
            paint: Paint::Tiling(Arc::clone(&white_screen)),
            alpha: 1.0,
            overprint: None,
        });

        let slide = display_list_to_slide(
            &dl,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide export");
        let png_bytes = slide
            .elements
            .iter()
            .find_map(|e| match e.base() {
                SlideElement::Image { image_data, .. } => Some(image_data.as_slice()),
                _ => None,
            })
            .expect("white Screen pattern over spaced text gap must emit opaque replacement PNG");
        let decoded = image::load_from_memory(png_bytes)
            .expect("valid PNG")
            .to_rgba8();
        let center = decoded.get_pixel(decoded.width() / 2, decoded.height() / 2);
        assert_eq!(
            center,
            &image::Rgba([255, 255, 255, 255]),
            "replacement PNG over spaced text gap must be opaque white to cover misplaced PowerPoint 'B', got {center:?}"
        );

        // When the same overlapped run uses an outline-less placeholder font,
        // the backdrop overlap guard must refuse to silently drop the text.
        let mut placeholder_fonts = FontCache::new();
        let ph_id = placeholder_fonts.insert(
            "F1".into(),
            LoadedFont::new_standard("Helvetica".into()).expect("Helvetica font"),
        );
        let mut ph_dl = dl.clone();
        if let RenderCommand::DrawGlyphRun(ref mut run) = ph_dl.commands[0] {
            run.font_id = ph_id;
        }
        assert!(
            display_list_to_slide(
                &ph_dl,
                0,
                &placeholder_fonts,
                &ImageCache::new(),
                &ConversionOptions::default(),
            )
            .is_err(),
            "backdrop tiling pattern overlapping outline-less native text must return an error"
        );
    }
}
